/* Standalone PipeWire source for the linux-capture job. fd 3 is an open
 * /dev/udmabuf; the test needs no graphics hardware or desktop permission. */
#define _GNU_SOURCE
#include <err.h>
#include <fcntl.h>
#include <linux/udmabuf.h>
#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

enum { WIDTH = 64, HEIGHT = 32, OFFSET = 37, STRIDE = WIDTH * 4 + 12 };

static struct pw_stream *stream;
static int plane;

static const struct spa_pod *offer(struct spa_pod_builder *b, enum spa_video_format format) {
    struct spa_video_info_raw info = {
        .format = format, .size = SPA_RECTANGLE(WIDTH, HEIGHT),
        .framerate = SPA_FRACTION(30, 1), .flags = SPA_VIDEO_FLAG_MODIFIER, .modifier = 0,
    };
    return spa_format_video_raw_build(b, SPA_PARAM_EnumFormat, &info);
}

static void add_buffer(void *unused, struct pw_buffer *buffer) {
    struct spa_buffer *b = buffer->buffer;
    if (b->n_datas != 1 || !(b->datas[0].type & (1u << SPA_DATA_DmaBuf)))
        errx(1, "expected a single DMA-BUF plane");
    /* Immutable picture: every buffer can reference the same allocation. */
    b->datas[0].type = SPA_DATA_DmaBuf;
    b->datas[0].flags = SPA_DATA_FLAG_READABLE;
    b->datas[0].fd = plane;
    b->datas[0].data = NULL;
    b->datas[0].mapoffset = 0;
    b->datas[0].maxsize = 0;
}

static void process(void *unused) {
    struct pw_buffer *buffer = pw_stream_dequeue_buffer(stream);
    if (!buffer) return;
    struct spa_chunk *chunk = buffer->buffer->datas[0].chunk;
    chunk->offset = OFFSET;
    chunk->stride = STRIDE;
    chunk->size = 1; /* Not a byte count for DMA-BUF. */
    chunk->flags = SPA_CHUNK_FLAG_NONE;
    pw_stream_queue_buffer(stream, buffer);
    // Change interpretation of the *same* bytes on the same live node.
    static unsigned frames;
    if (++frames == 20) {
        uint8_t bytes[512];
        struct spa_pod_builder b = SPA_POD_BUILDER_INIT(bytes, sizeof bytes);
        const struct spa_pod *next = offer(&b, SPA_VIDEO_FORMAT_RGBA);
        if (pw_stream_update_params(stream, &next, 1) < 0) errx(1, "re-offer RGBA");
    }
}

static void format_changed(void *unused, uint32_t id, const struct spa_pod *param) {
    if (id != SPA_PARAM_Format || !param) return;
    struct spa_video_info_raw raw = {0};
    if (spa_format_video_raw_parse(param, &raw) < 0 ||
        !(raw.flags & SPA_VIDEO_FLAG_MODIFIER) || raw.modifier != 0 ||
        (raw.format != SPA_VIDEO_FORMAT_BGRA && raw.format != SPA_VIDEO_FORMAT_RGBA))
        errx(1, "expected explicitly LINEAR BGRA/RGBA");
    uint8_t bytes[512];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(bytes, sizeof bytes);
    const struct spa_pod *buffers = spa_pod_builder_add_object(&b,
        SPA_TYPE_OBJECT_ParamBuffers, SPA_PARAM_Buffers,
        SPA_PARAM_BUFFERS_buffers, SPA_POD_Int(4), SPA_PARAM_BUFFERS_blocks, SPA_POD_Int(1),
        SPA_PARAM_BUFFERS_dataType, SPA_POD_CHOICE_FLAGS_Int(1u << SPA_DATA_DmaBuf));
    if (pw_stream_update_params(stream, &buffers, 1) < 0) errx(1, "update buffers");
}

static void state_changed(void *unused, enum pw_stream_state old, enum pw_stream_state state,
                          const char *error) {
    if (state == PW_STREAM_STATE_ERROR) errx(1, "stream: %s", error);
    if (state == PW_STREAM_STATE_PAUSED && old == PW_STREAM_STATE_CONNECTING) {
        printf("%u\n", pw_stream_get_node_id(stream));
        fflush(stdout);
    }
}

int main(void) {
    alarm(60);
    long page = sysconf(_SC_PAGESIZE);
    size_t len = ((OFFSET + STRIDE * HEIGHT + page - 1) / page) * page;
    int memory = memfd_create("linear-dmabuf-pattern", MFD_ALLOW_SEALING | MFD_CLOEXEC);
    if (memory < 0 || ftruncate(memory, len) || fcntl(memory, F_ADD_SEALS, F_SEAL_SHRINK))
        err(1, "memfd");
    uint8_t *p = mmap(NULL, len, PROT_WRITE, MAP_SHARED, memory, 0);
    if (p == MAP_FAILED) err(1, "mmap memfd");
    memset(p, 0xa5, len);
    for (int y = 0; y < HEIGHT; ++y) {
        for (int x = 0; x < WIDTH; ++x) {
            uint8_t bgra[] = { x, y, x ^ y, 255 };
            memcpy(p + OFFSET + STRIDE * y + x * 4, bgra, 4);
        }
    }
    munmap(p, len);
    struct udmabuf_create create = { .memfd = memory, .size = len, .flags = UDMABUF_FLAGS_CLOEXEC };
    plane = ioctl(3, UDMABUF_CREATE, &create);
    if (plane < 0) err(1, "UDMABUF_CREATE");
    close(memory);
    close(3);

    pw_init(NULL, NULL);
    struct pw_main_loop *loop = pw_main_loop_new(NULL);
    static const struct pw_stream_events events = {
        PW_VERSION_STREAM_EVENTS, .add_buffer = add_buffer, .process = process,
        .param_changed = format_changed, .state_changed = state_changed,
    };
    stream = pw_stream_new_simple(pw_main_loop_get_loop(loop), "linear-dmabuf-test",
        pw_properties_new(PW_KEY_MEDIA_CLASS, "Video/Source", NULL), &events, NULL);
    if (!stream) err(1, "create source");
    uint8_t bytes[512];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(bytes, sizeof bytes);
    const struct spa_pod *format = offer(&b, SPA_VIDEO_FORMAT_BGRA);
    if (pw_stream_connect(stream, PW_DIRECTION_OUTPUT, PW_ID_ANY, PW_STREAM_FLAG_ALLOC_BUFFERS,
                          &format, 1) < 0) err(1, "connect source");
    pw_main_loop_run(loop);
    return 0;
}
