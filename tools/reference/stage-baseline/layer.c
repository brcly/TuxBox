// Time each pipeline the driver compiles, for the per-stage baseline.
//
// vkd3d-proton converts DXIL to SPIR-V inside CreateComputePipelineState and
// then calls vkCreateComputePipelines, which is the driver's compile. This
// layer appends one line per call to the file named by TUXBOX_PIPELINE_TIMES:
//
//   driver KIND START_NS END_NS RESULT COUNT HWM0 RSS0 HWM1 RSS1
//   destroyed AT_NS RSS
//
// Times are CLOCK_REALTIME, so they line up with the guest's
// GetSystemTimePreciseAsFileTime. HWM and RSS are VmHWM and VmRSS in KiB, read
// before and after the call. After each call and each destroyed pipeline the
// peak is reset, so the next HWM0 is the peak since then: what vkd3d-proton
// used while converting the next program. The layer changes no arguments.
#define _GNU_SOURCE
#include <vulkan/vk_layer.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

struct dispatch {
    void *key;
    PFN_vkGetInstanceProcAddr instance_proc;
    PFN_vkGetDeviceProcAddr device_proc;
    PFN_vkDestroyInstance destroy_instance;
    PFN_vkDestroyDevice destroy_device;
    PFN_vkCreateComputePipelines compute;
    PFN_vkCreateGraphicsPipelines graphics;
    PFN_vkDestroyPipeline destroy_pipeline;
    struct dispatch *next;
};

static struct dispatch *instances, *devices;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static int log_fd = -2;

static void *dispatch_key(const void *object) {
    void *key;
    memcpy(&key, object, sizeof(key));
    return key;
}

static struct dispatch *find(struct dispatch *list, const void *object) {
    void *key = dispatch_key(object);
    pthread_mutex_lock(&lock);
    while (list && list->key != key)
        list = list->next;
    pthread_mutex_unlock(&lock);
    return list;
}

static void add(struct dispatch **list, struct dispatch *entry) {
    pthread_mutex_lock(&lock);
    entry->next = *list;
    *list = entry;
    pthread_mutex_unlock(&lock);
}

static struct dispatch *take(struct dispatch **list, const void *object) {
    void *key = dispatch_key(object);
    pthread_mutex_lock(&lock);
    struct dispatch **link = list;
    while (*link && (*link)->key != key)
        link = &(*link)->next;
    struct dispatch *entry = *link;
    if (entry)
        *link = entry->next;
    pthread_mutex_unlock(&lock);
    return entry;
}

static long long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

// VmHWM and VmRSS of this process, in KiB; -1 when unreadable.
static void memory(long *hwm, long *rss) {
    *hwm = *rss = -1;
    FILE *status = fopen("/proc/self/status", "r");
    if (!status)
        return;
    char line[256];
    while (fgets(line, sizeof(line), status)) {
        if (!strncmp(line, "VmHWM:", 6))
            *hwm = strtol(line + 6, NULL, 10);
        else if (!strncmp(line, "VmRSS:", 6))
            *rss = strtol(line + 6, NULL, 10);
    }
    fclose(status);
}

static void reset_peak(void) {
    int fd = open("/proc/self/clear_refs", O_WRONLY | O_CLOEXEC);
    if (fd >= 0) {
        // "5" resets VmHWM to the current resident size.
        if (write(fd, "5", 1) != 1) {
        }
        close(fd);
    }
}

static void record(const char *line, size_t length) {
    pthread_mutex_lock(&lock);
    if (log_fd == -2) {
        const char *path = getenv("TUXBOX_PIPELINE_TIMES");
        log_fd = path ? open(path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0600) : -1;
    }
    if (log_fd >= 0 && write(log_fd, line, length) != (ssize_t)length) {
    }
    pthread_mutex_unlock(&lock);
}

static void timed(const char *kind, long long start, VkResult result, uint32_t count, long hwm0, long rss0) {
    long long end = now_ns();
    long hwm1, rss1;
    memory(&hwm1, &rss1);
    char line[256];
    int length = snprintf(line, sizeof(line), "driver %s %lld %lld %d %u %ld %ld %ld %ld\n", kind, start, end,
                          (int)result, count, hwm0, rss0, hwm1, rss1);
    if (length > 0 && (size_t)length < sizeof(line))
        record(line, (size_t)length);
    reset_peak();
}

static VKAPI_ATTR VkResult VKAPI_CALL create_compute(VkDevice device, VkPipelineCache cache, uint32_t count,
        const VkComputePipelineCreateInfo *infos, const VkAllocationCallbacks *allocator, VkPipeline *pipelines) {
    const struct dispatch *entry = find(devices, device);
    long hwm0, rss0;
    memory(&hwm0, &rss0);
    long long start = now_ns();
    VkResult result = entry->compute(device, cache, count, infos, allocator, pipelines);
    timed("compute", start, result, count, hwm0, rss0);
    return result;
}

static VKAPI_ATTR VkResult VKAPI_CALL create_graphics(VkDevice device, VkPipelineCache cache, uint32_t count,
        const VkGraphicsPipelineCreateInfo *infos, const VkAllocationCallbacks *allocator, VkPipeline *pipelines) {
    const struct dispatch *entry = find(devices, device);
    long hwm0, rss0;
    memory(&hwm0, &rss0);
    long long start = now_ns();
    VkResult result = entry->graphics(device, cache, count, infos, allocator, pipelines);
    timed("graphics", start, result, count, hwm0, rss0);
    return result;
}

static VKAPI_ATTR void VKAPI_CALL destroy_pipeline(VkDevice device, VkPipeline pipeline,
        const VkAllocationCallbacks *allocator) {
    const struct dispatch *entry = find(devices, device);
    entry->destroy_pipeline(device, pipeline, allocator);
    if (pipeline == VK_NULL_HANDLE)
        return;
    long hwm, rss;
    memory(&hwm, &rss);
    char line[96];
    int length = snprintf(line, sizeof(line), "destroyed %lld %ld\n", now_ns(), rss);
    if (length > 0 && (size_t)length < sizeof(line))
        record(line, (size_t)length);
    reset_peak();
}

static VKAPI_ATTR VkResult VKAPI_CALL create_instance(const VkInstanceCreateInfo *info,
        const VkAllocationCallbacks *allocator, VkInstance *instance) {
    VkLayerInstanceCreateInfo *chain = (VkLayerInstanceCreateInfo *)info->pNext;
    while (chain && (chain->sType != VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO ||
                     chain->function != VK_LAYER_LINK_INFO))
        chain = (VkLayerInstanceCreateInfo *)chain->pNext;
    if (!chain || !chain->u.pLayerInfo)
        return VK_ERROR_INITIALIZATION_FAILED;
    PFN_vkGetInstanceProcAddr next = chain->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    PFN_vkCreateInstance create = (PFN_vkCreateInstance)next(NULL, "vkCreateInstance");
    struct dispatch *entry = calloc(1, sizeof(*entry));
    if (!create || !entry) {
        free(entry);
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    chain->u.pLayerInfo = chain->u.pLayerInfo->pNext;
    VkResult result = create(info, allocator, instance);
    if (result != VK_SUCCESS) {
        free(entry);
        return result;
    }
    entry->key = dispatch_key(*instance);
    entry->instance_proc = next;
    entry->destroy_instance = (PFN_vkDestroyInstance)next(*instance, "vkDestroyInstance");
    add(&instances, entry);
    return result;
}

static VKAPI_ATTR void VKAPI_CALL destroy_instance(VkInstance instance, const VkAllocationCallbacks *allocator) {
    if (!instance)
        return;
    struct dispatch *entry = take(&instances, instance);
    if (entry) {
        entry->destroy_instance(instance, allocator);
        free(entry);
    }
}

static VKAPI_ATTR VkResult VKAPI_CALL create_device(VkPhysicalDevice physical, const VkDeviceCreateInfo *info,
        const VkAllocationCallbacks *allocator, VkDevice *device) {
    VkLayerDeviceCreateInfo *chain = (VkLayerDeviceCreateInfo *)info->pNext;
    while (chain && (chain->sType != VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO ||
                     chain->function != VK_LAYER_LINK_INFO))
        chain = (VkLayerDeviceCreateInfo *)chain->pNext;
    if (!chain || !chain->u.pLayerInfo)
        return VK_ERROR_INITIALIZATION_FAILED;
    PFN_vkGetInstanceProcAddr instance_proc = chain->u.pLayerInfo->pfnNextGetInstanceProcAddr;
    PFN_vkGetDeviceProcAddr device_proc = chain->u.pLayerInfo->pfnNextGetDeviceProcAddr;
    PFN_vkCreateDevice create = (PFN_vkCreateDevice)instance_proc(NULL, "vkCreateDevice");
    struct dispatch *entry = calloc(1, sizeof(*entry));
    if (!create || !entry) {
        free(entry);
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    chain->u.pLayerInfo = chain->u.pLayerInfo->pNext;
    VkResult result = create(physical, info, allocator, device);
    if (result != VK_SUCCESS) {
        free(entry);
        return result;
    }
    entry->key = dispatch_key(*device);
    entry->device_proc = device_proc;
    entry->destroy_device = (PFN_vkDestroyDevice)device_proc(*device, "vkDestroyDevice");
    entry->compute = (PFN_vkCreateComputePipelines)device_proc(*device, "vkCreateComputePipelines");
    entry->graphics = (PFN_vkCreateGraphicsPipelines)device_proc(*device, "vkCreateGraphicsPipelines");
    entry->destroy_pipeline = (PFN_vkDestroyPipeline)device_proc(*device, "vkDestroyPipeline");
    add(&devices, entry);
    reset_peak();
    return result;
}

static VKAPI_ATTR void VKAPI_CALL destroy_device(VkDevice device, const VkAllocationCallbacks *allocator) {
    if (!device)
        return;
    struct dispatch *entry = take(&devices, device);
    if (entry) {
        entry->destroy_device(device, allocator);
        free(entry);
    }
}

static VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL get_device_proc(VkDevice device, const char *name) {
    if (!strcmp(name, "vkGetDeviceProcAddr"))
        return (PFN_vkVoidFunction)get_device_proc;
    if (!device)
        return NULL;
    const struct dispatch *entry = find(devices, device);
    if (!entry)
        return NULL;
    if (!strcmp(name, "vkDestroyDevice"))
        return (PFN_vkVoidFunction)destroy_device;
    if (!strcmp(name, "vkCreateComputePipelines"))
        return entry->compute ? (PFN_vkVoidFunction)create_compute : NULL;
    if (!strcmp(name, "vkCreateGraphicsPipelines"))
        return entry->graphics ? (PFN_vkVoidFunction)create_graphics : NULL;
    if (!strcmp(name, "vkDestroyPipeline"))
        return entry->destroy_pipeline ? (PFN_vkVoidFunction)destroy_pipeline : NULL;
    return entry->device_proc(device, name);
}

static VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL get_instance_proc(VkInstance instance, const char *name) {
    if (!strcmp(name, "vkCreateInstance"))
        return (PFN_vkVoidFunction)create_instance;
    if (!strcmp(name, "vkGetInstanceProcAddr"))
        return (PFN_vkVoidFunction)get_instance_proc;
    if (!strcmp(name, "vkGetDeviceProcAddr"))
        return (PFN_vkVoidFunction)get_device_proc;
    if (!instance)
        return NULL;
    const struct dispatch *entry = find(instances, instance);
    if (!entry)
        return NULL;
    if (!strcmp(name, "vkDestroyInstance"))
        return (PFN_vkVoidFunction)destroy_instance;
    if (!strcmp(name, "vkCreateDevice"))
        return (PFN_vkVoidFunction)create_device;
    return entry->instance_proc(instance, name);
}

VKAPI_ATTR VkResult VKAPI_CALL vkNegotiateLoaderLayerInterfaceVersion(VkNegotiateLayerInterface *info) {
    if (!info || info->sType != LAYER_NEGOTIATE_INTERFACE_STRUCT || info->loaderLayerInterfaceVersion < 2)
        return VK_ERROR_INITIALIZATION_FAILED;
    info->loaderLayerInterfaceVersion = 2;
    info->pfnGetInstanceProcAddr = get_instance_proc;
    info->pfnGetDeviceProcAddr = get_device_proc;
    info->pfnGetPhysicalDeviceProcAddr = NULL;
    return VK_SUCCESS;
}
