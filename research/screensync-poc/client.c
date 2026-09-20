// screensync-poc: minimal PipeWire consumer that binds gamescope's private
// "gamescope" Video/Source node and negotiates a small requested_size using
// gamescope's own vendor SPA extension (SPA_FORMAT_VIDEO_requested_size,
// enum value 0x70000 — mirrored from gamescope's src/pipewire_gamescope.hpp,
// NOT a standard PipeWire property).
//
// Research prototype only. Prints frame timing + a cheap edge-average color
// so we can eyeball correctness, and reports negotiated size/format/buffer
// type (MemFd vs DmaBuf).

#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>
#include <spa/param/video/raw.h>
#include <spa/param/props.h>
#include <spa/debug/format.h>
#include <spa/utils/result.h>

#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <time.h>
#include <math.h>

// --- gamescope vendor SPA extension (mirrored from pipewire_gamescope.hpp) ---
enum {
    SPA_FORMAT_VIDEO_requested_size = 0x70000,
    SPA_FORMAT_VIDEO_gamescope_focus_appid = 0x70001,
};
enum {
    SPA_META_requested_size_scale = 0x70000
};
struct spa_gamescope {
    struct spa_rectangle requested_size;
    uint64_t focus_appid;
};

// --- config (env-overridable) ---
static uint32_t g_req_w = 24;
static uint32_t g_req_h = 40;
static int g_max_frames = 300; // stop after N frames processed (0 = unlimited)

struct data {
    struct pw_main_loop *loop;
    struct pw_stream *stream;
    struct spa_video_info_raw format;
    int frame_count;
    int negotiations;
    struct timespec t_connect;
    struct timespec t_first_frame;
    struct timespec t_last_frame;
    int got_first_small_frame;
    uint32_t last_w, last_h;
};

static double ts_diff_ms(struct timespec *a, struct timespec *b) {
    return (b->tv_sec - a->tv_sec) * 1000.0 + (b->tv_nsec - a->tv_nsec) / 1e6;
}

static void on_process(void *userdata) {
    struct data *d = userdata;
    struct pw_buffer *b;
    struct spa_buffer *buf;

    if ((b = pw_stream_dequeue_buffer(d->stream)) == NULL) {
        pw_log_warn("out of buffers");
        return;
    }

    buf = b->buffer;
    struct spa_data *sd = &buf->datas[0];

    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    if (d->frame_count == 0)
        d->t_first_frame = now;
    d->t_last_frame = now;

    uint32_t w = d->format.size.width, h = d->format.size.height;
    d->last_w = w; d->last_h = h;

    // Only bother reading pixels once we've converged to our small requested size
    // (the first frame(s) arrive at gamescope's initial full-res proposal).
    if (sd->data != NULL && w <= 256 && h <= 256) {
        uint8_t *pix = (uint8_t *) sd->data;
        int stride = sd->chunk->stride;
        // BGRx: sample left edge column + right edge column, average.
        uint64_t bl=0,gl=0,rl=0,nl=0, br=0,gr=0,rr=0,nr=0;
        for (uint32_t y = 0; y < h; y++) {
            uint8_t *row = pix + (size_t)y * stride;
            // left edge = first pixel
            bl += row[0]; gl += row[1]; rl += row[2]; nl++;
            // right edge = last pixel
            uint8_t *rpix = row + (w - 1) * 4;
            br += rpix[0]; gr += rpix[1]; rr += rpix[2]; nr++;
        }
        if (!d->got_first_small_frame) {
            d->got_first_small_frame = 1;
            printf("[%s] FIRST SMALL FRAME at frame#%d size=%ux%u type=%s stride=%d\n",
                __TIME__, d->frame_count, w, h,
                (sd->type == SPA_DATA_DmaBuf) ? "DmaBuf" : (sd->type == SPA_DATA_MemFd ? "MemFd" : "other"),
                stride);
        }
        if (d->frame_count % 30 == 0) {
            printf("frame#%d size=%ux%u LEFT rgb=(%llu,%llu,%llu) RIGHT rgb=(%llu,%llu,%llu)\n",
                d->frame_count, w, h,
                (unsigned long long)(rl/nl), (unsigned long long)(gl/nl), (unsigned long long)(bl/nl),
                (unsigned long long)(rr/nr), (unsigned long long)(gr/nr), (unsigned long long)(br/nr));
        }
    } else if (d->frame_count % 30 == 0) {
        printf("frame#%d size=%ux%u type=%s data=%p (not sampling, too big or null)\n",
            d->frame_count, w, h,
            (sd->type == SPA_DATA_DmaBuf) ? "DmaBuf" : (sd->type == SPA_DATA_MemFd ? "MemFd" : "other"),
            sd->data);
    }

    d->frame_count++;
    pw_stream_queue_buffer(d->stream, b);

    if (g_max_frames > 0 && d->frame_count >= g_max_frames) {
        pw_main_loop_quit(d->loop);
    }
}

static void on_state_changed(void *userdata, enum pw_stream_state old,
                              enum pw_stream_state state, const char *error) {
    (void) old;
    printf("[state] %s -> %s%s%s\n", pw_stream_state_as_string(old), pw_stream_state_as_string(state),
        error ? " error: " : "", error ? error : "");
}

static void on_param_changed(void *userdata, uint32_t id, const struct spa_pod *param) {
    struct data *d = userdata;

    if (param == NULL || id != SPA_PARAM_Format)
        return;

    struct spa_gamescope gsinfo = {0};
    // Parse using the SAME custom fields gamescope's own producer parses,
    // so we can confirm what it negotiated (and what it thinks we asked for).
    spa_pod_parse_object(param,
        SPA_TYPE_OBJECT_Format, NULL,
        SPA_FORMAT_VIDEO_format,   SPA_POD_Id(&d->format.format),
        SPA_FORMAT_VIDEO_size,     SPA_POD_Rectangle(&d->format.size),
        SPA_FORMAT_VIDEO_framerate, SPA_POD_OPT_Fraction(&d->format.framerate),
        SPA_FORMAT_VIDEO_requested_size, SPA_POD_OPT_Rectangle(&gsinfo.requested_size),
        SPA_FORMAT_VIDEO_gamescope_focus_appid, SPA_POD_OPT_Long(&gsinfo.focus_appid));

    d->negotiations++;
    printf("[negotiation #%d] format=%d size=%ux%u framerate=%u/%u gamescope_requested_size_seen=%ux%u focus_appid=%llu\n",
        d->negotiations, d->format.format, d->format.size.width, d->format.size.height,
        d->format.framerate.num, d->format.framerate.denom,
        gsinfo.requested_size.width, gsinfo.requested_size.height,
        (unsigned long long) gsinfo.focus_appid);
    // NOTE: requested_size is now sent as part of our EnumFormat proposal at
    // connect time (see main()), not pushed here. An earlier attempt to
    // piggyback it via a manual SPA_PARAM_Format reply from the consumer
    // side was a no-op (no 2nd negotiation ever happened) -- SPA_PARAM_Format
    // is a negotiation RESULT, not a channel for a consumer to inject extra
    // custom fields after the fact.
}

static const struct pw_stream_events stream_events = {
    PW_VERSION_STREAM_EVENTS,
    .state_changed = on_state_changed,
    .param_changed = on_param_changed,
    .process = on_process,
};

int main(int argc, char *argv[]) {
    struct data data = {0};
    struct pw_properties *props;

    if (argc > 1) g_req_w = (uint32_t) atoi(argv[1]);
    if (argc > 2) g_req_h = (uint32_t) atoi(argv[2]);
    if (argc > 3) g_max_frames = atoi(argv[3]);

    setvbuf(stdout, NULL, _IOLBF, 0);
    pw_init(&argc, &argv);

    data.loop = pw_main_loop_new(NULL);

    props = pw_properties_new(
        PW_KEY_MEDIA_TYPE, "Video",
        PW_KEY_MEDIA_CATEGORY, "Capture",
        PW_KEY_MEDIA_ROLE, "Screen",
        PW_KEY_TARGET_OBJECT, "gamescope",
        NULL);

    data.stream = pw_stream_new_simple(
        pw_main_loop_get_loop(data.loop),
        "screensync-poc",
        props,
        &stream_events,
        &data);

    uint8_t buffer[1024];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));
    const struct spa_pod *params[1];

    // Propose: BGRx, any size (choice range), any framerate. We deliberately
    // do NOT try to lowball the size here -- gamescope's first EnumFormat is
    // a *fixed* full-resolution rectangle, so a range on our side just means
    // "we accept whatever you propose." The actual shrink request travels
    // via our reply's custom requested_size field (see on_param_changed).
    struct spa_rectangle def_size = SPA_RECTANGLE(320, 240);
    struct spa_rectangle min_size = SPA_RECTANGLE(1, 1);
    struct spa_rectangle max_size = SPA_RECTANGLE(8192, 8192);
    struct spa_fraction def_rate = SPA_FRACTION(0, 1);

    struct spa_rectangle want = SPA_RECTANGLE(g_req_w, g_req_h);

    if (g_req_w != 0 && g_req_h != 0) {
        params[0] = spa_pod_builder_add_object(&b,
            SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat,
            SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video),
            SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
            SPA_FORMAT_VIDEO_format, SPA_POD_Id(SPA_VIDEO_FORMAT_BGRx),
            SPA_FORMAT_VIDEO_size, SPA_POD_CHOICE_RANGE_Rectangle(&def_size, &min_size, &max_size),
            SPA_FORMAT_VIDEO_framerate, SPA_POD_CHOICE_RANGE_Fraction(&def_rate, &def_rate, &SPA_FRACTION(1000,1)),
            // The actual downscale trigger: a FIXED rectangle here, intersected
            // against gamescope's wide-open CHOICE_RANGE(0,0 .. UINT32_MAX) for
            // this same vendor field, should pin the negotiated value to ours.
            SPA_FORMAT_VIDEO_requested_size, SPA_POD_Rectangle(&want));
    } else {
        // req size 0x0 => don't send the vendor field at all (matches the
        // known-working full-native-res baseline; used to A/B the GPU/CPU
        // cost of sustained per-vblank paint_pipewire() regardless of the
        // (separately debugged) downscale negotiation.
        params[0] = spa_pod_builder_add_object(&b,
            SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat,
            SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video),
            SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
            SPA_FORMAT_VIDEO_format, SPA_POD_Id(SPA_VIDEO_FORMAT_BGRx),
            SPA_FORMAT_VIDEO_size, SPA_POD_CHOICE_RANGE_Rectangle(&def_size, &min_size, &max_size),
            SPA_FORMAT_VIDEO_framerate, SPA_POD_CHOICE_RANGE_Fraction(&def_rate, &def_rate, &SPA_FRACTION(1000,1)));
    }

    clock_gettime(CLOCK_MONOTONIC, &data.t_connect);

    int ret = pw_stream_connect(data.stream,
        PW_DIRECTION_INPUT,
        PW_ID_ANY,
        PW_STREAM_FLAG_AUTOCONNECT |
        PW_STREAM_FLAG_MAP_BUFFERS,
        params, 1);

    if (ret < 0) {
        fprintf(stderr, "can't connect: %s\n", spa_strerror(ret));
        return -1;
    }

    printf("connecting to target-object=gamescope, requesting size %ux%u, max_frames=%d...\n",
        g_req_w, g_req_h, g_max_frames);

    pw_main_loop_run(data.loop);

    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    printf("\n=== SUMMARY ===\n");
    printf("frames processed: %d\n", data.frame_count);
    printf("negotiations: %d\n", data.negotiations);
    printf("final size: %ux%u\n", data.last_w, data.last_h);
    if (data.frame_count > 1) {
        double span_ms = ts_diff_ms(&data.t_first_frame, &data.t_last_frame);
        printf("time first->last frame: %.1f ms over %d frames => %.1f fps avg\n",
            span_ms, data.frame_count - 1, (data.frame_count - 1) * 1000.0 / span_ms);
    }
    printf("time connect->first frame: %.1f ms\n", ts_diff_ms(&data.t_connect, &data.t_first_frame));

    pw_stream_destroy(data.stream);
    pw_main_loop_destroy(data.loop);
    pw_deinit();

    return 0;
}
