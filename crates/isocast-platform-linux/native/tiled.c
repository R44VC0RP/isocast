/* The desktop build already has Vulkan headers via GTK. Minimal builds do
 * not need them. Use only the actual instance/device dispatch from wgpu: no
 * additional loader, linked driver, instance, device or queue. */
#include <stdint.h>
#include <stdlib.h>
#if __has_include(<vulkan/vulkan_core.h>)
#include <vulkan/vulkan_core.h>
#endif

struct modifier {
    uint64_t value;
    uint32_t width, height;
};

int iso_tiled_available(void) {
#if defined(VK_EXT_physical_device_drm) && defined(VK_EXT_image_drm_format_modifier) &&            \
    defined(VK_EXT_queue_family_foreign)
    return 1;
#else
    return 0;
#endif
}

int iso_tiled_render_node(const void *phys, const void *get, uint32_t *major, uint32_t *minor) {
#ifdef VK_EXT_physical_device_drm
    VkPhysicalDeviceDrmPropertiesEXT drm = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_DRM_PROPERTIES_EXT};
    VkPhysicalDeviceProperties2 p = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_PROPERTIES_2,
                                     .pNext = &drm};
    ((PFN_vkGetPhysicalDeviceProperties2)get)(*(const VkPhysicalDevice *)phys, &p);
    if (drm.hasRender && drm.renderMajor >= 0 && drm.renderMinor >= 0) {
        *major = drm.renderMajor;
        *minor = drm.renderMinor;
        return 1;
    }
#endif
    return 0;
}

uint32_t iso_tiled_modifiers(const void *handle, const void *formats, const void *images, int bgra,
                             struct modifier *out, uint32_t cap) {
    uint32_t count = 0;
#ifdef VK_EXT_image_drm_format_modifier
    VkPhysicalDevice phys = *(const VkPhysicalDevice *)handle;
    VkFormat format = bgra ? VK_FORMAT_B8G8R8A8_UNORM : VK_FORMAT_R8G8B8A8_UNORM;
    VkDrmFormatModifierPropertiesListEXT list = {
        .sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT};
    VkFormatProperties2 p = {.sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2, .pNext = &list};
    PFN_vkGetPhysicalDeviceFormatProperties2 get =
        (PFN_vkGetPhysicalDeviceFormatProperties2)formats;
    get(phys, format, &p);
    if (!list.drmFormatModifierCount || list.drmFormatModifierCount > 4096)
        return 0;
    list.pDrmFormatModifierProperties =
        calloc(list.drmFormatModifierCount, sizeof *list.pDrmFormatModifierProperties);
    if (!list.pDrmFormatModifierProperties)
        return 0;
    get(phys, format, &p);
    for (uint32_t i = 0; i < list.drmFormatModifierCount && count < cap; i++) {
        const VkDrmFormatModifierPropertiesEXT *m = &list.pDrmFormatModifierProperties[i];
        if (!m->drmFormatModifier || m->drmFormatModifier == UINT64_C(0x00ffffffffffffff) ||
            m->drmFormatModifierPlaneCount != 1 ||
            !(m->drmFormatModifierTilingFeatures & VK_FORMAT_FEATURE_TRANSFER_SRC_BIT))
            continue;
        VkPhysicalDeviceImageDrmFormatModifierInfoEXT mod = {
            .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT,
            .drmFormatModifier = m->drmFormatModifier,
            .sharingMode = VK_SHARING_MODE_EXCLUSIVE};
        VkPhysicalDeviceExternalImageFormatInfo ext = {
            .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO,
            .pNext = &mod,
            .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
        VkPhysicalDeviceImageFormatInfo2 info = {
            .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
            .pNext = &ext,
            .format = format,
            .type = VK_IMAGE_TYPE_2D,
            .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
            .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT};
        VkExternalImageFormatProperties external = {
            .sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES};
        VkImageFormatProperties2 props = {.sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2,
                                          .pNext = &external};
        if (((PFN_vkGetPhysicalDeviceImageFormatProperties2)images)(phys, &info, &props) !=
                VK_SUCCESS ||
            !(external.externalMemoryProperties.externalMemoryFeatures &
              VK_EXTERNAL_MEMORY_FEATURE_IMPORTABLE_BIT) ||
            !(props.imageFormatProperties.sampleCounts & VK_SAMPLE_COUNT_1_BIT))
            continue;
        out[count++] =
            (struct modifier){m->drmFormatModifier, props.imageFormatProperties.maxExtent.width,
                              props.imageFormatProperties.maxExtent.height};
    }
    free(list.pDrmFormatModifierProperties);
#endif
    return count;
}

void iso_tiled_copy(const void *command, const void *barriers, const void *copy, uint32_t family,
                    const void *image, const void *buffer, uint32_t w, uint32_t h) {
#ifdef VK_EXT_queue_family_foreign
    VkCommandBuffer cmd = *(const VkCommandBuffer *)command;
    PFN_vkCmdPipelineBarrier barrier = (PFN_vkCmdPipelineBarrier)barriers;
    VkImageMemoryBarrier b = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
                              .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
                              .oldLayout = VK_IMAGE_LAYOUT_GENERAL,
                              .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
                              .srcQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT,
                              .dstQueueFamilyIndex = family,
                              .image = *(const VkImage *)image,
                              .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
    barrier(cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0,
            NULL, 1, &b);
    VkBufferImageCopy region = {.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
                                .imageExtent = {w, h, 1}};
    ((PFN_vkCmdCopyImageToBuffer)copy)(cmd, b.image, b.newLayout, *(const VkBuffer *)buffer, 1,
                                       &region);
    b.srcAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
    b.dstAccessMask = 0;
    b.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
    b.newLayout = VK_IMAGE_LAYOUT_GENERAL;
    b.srcQueueFamilyIndex = family;
    b.dstQueueFamilyIndex = VK_QUEUE_FAMILY_FOREIGN_EXT;
    barrier(cmd, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0, NULL,
            0, NULL, 1, &b);
#endif
}

void iso_tiled_transfer_to_host(const void *command, const void *barriers) {
#ifdef VK_VERSION_1_0
    VkMemoryBarrier host = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
                            .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
                            .dstAccessMask = VK_ACCESS_HOST_READ_BIT};
    ((PFN_vkCmdPipelineBarrier)barriers)(*(const VkCommandBuffer *)command,
                                         VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT,
                                         0, 1, &host, 0, NULL, 0, NULL);
#endif
}
