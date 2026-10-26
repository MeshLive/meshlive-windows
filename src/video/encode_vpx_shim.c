/* MeshLive: a narrow C boundary over the pinned static libvpx VP9 encoder
 * (software VP9 for PCs without a hardware VP9 MFT). Android has no software
 * VP9 encoder; its MediaCodec VP9 path asks for CBR, no B-frames, keyframes
 * only on request plus the 60 s safety keyframe, and no error resilience or
 * layers by default. The libvpx settings are WebRTC's real-time VP9 ones
 * (LibvpxVp9Encoder: one pass, CBR, lag 0, q 2..52, cyclic-refresh AQ, row MT,
 * tile columns by threads, static threshold 1, no frame-parallel mode) with
 * Android's rate buffer (1 s, initial/optimal 600 ms, like the libaom arm)
 * and no frame dropping (no loss by default). Auto alt-ref is off and the lag
 * is 0, so there are no hidden frames and no superframes: every input picture
 * gives exactly one shown frame.
 * SPDX-License-Identifier: GPL-3.0-or-later */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include "vpx/vp8cx.h"
#include "vpx/vpx_encoder.h"

typedef struct MeshVpxConfig {
  uint32_t width;
  uint32_t height;
  uint32_t fps;
  uint32_t bitrate_kbps;
  uint32_t cpu_used;
  uint32_t threads;
  uint32_t rate_buffer_ms;
} MeshVpxConfig;

typedef void (*MeshVpxSink)(void *context, const uint8_t *data, size_t size,
                            int64_t pts_us, int key_frame);

typedef struct MeshVpx {
  vpx_codec_ctx_t codec;
  vpx_codec_enc_cfg_t config;
  uint32_t width;
  uint32_t height;
  uint32_t fps;
} MeshVpx;

static int control(MeshVpx *e, int id, int value) {
  return vpx_codec_control_(&e->codec, id, value) == VPX_CODEC_OK;
}

const char *meshlive_vpx_version(void) { return vpx_codec_version_str(); }

void meshlive_vpx_close(MeshVpx *e) {
  if (e == NULL) return;
  vpx_codec_destroy(&e->codec);
  free(e);
}

static int log2_floor(uint32_t value) {
  int result = 0;
  while (value > 1) {
    value >>= 1;
    result++;
  }
  return result;
}

MeshVpx *meshlive_vpx_open(const MeshVpxConfig *c) {
  MeshVpx *e;
  if (c == NULL || c->width == 0 || c->height == 0 || (c->width & 1) ||
      (c->height & 1) || c->width > 4096 || c->height > 4096 || c->fps == 0 ||
      c->fps > 120 || c->bitrate_kbps == 0 || c->bitrate_kbps > 100000 ||
      c->cpu_used < 5 || c->cpu_used > 9 || c->threads == 0 ||
      c->threads > 16 || c->rate_buffer_ms < 100 || c->rate_buffer_ms > 6000)
    return NULL;
  e = (MeshVpx *)calloc(1, sizeof(MeshVpx));
  if (e == NULL) return NULL;
  if (vpx_codec_enc_config_default(vpx_codec_vp9_cx(), &e->config, 0) !=
      VPX_CODEC_OK) {
    free(e);
    return NULL;
  }
  e->width = c->width;
  e->height = c->height;
  e->fps = c->fps;
  e->config.g_w = c->width;
  e->config.g_h = c->height;
  e->config.g_threads = c->threads;
  e->config.g_timebase.num = 1;
  e->config.g_timebase.den = 1000000;
  e->config.g_profile = 0;
  e->config.g_input_bit_depth = 8;
  e->config.g_bit_depth = VPX_BITS_8;
  e->config.g_pass = VPX_RC_ONE_PASS;
  e->config.g_lag_in_frames = 0;
  e->config.g_error_resilient = 0;
  e->config.kf_mode = VPX_KF_DISABLED;
  e->config.rc_end_usage = VPX_CBR;
  e->config.rc_target_bitrate = c->bitrate_kbps;
  e->config.rc_min_quantizer = 2;
  e->config.rc_max_quantizer = 52;
  e->config.rc_undershoot_pct = 50;
  e->config.rc_overshoot_pct = 50;
  e->config.rc_buf_initial_sz = c->rate_buffer_ms < 600 ? c->rate_buffer_ms : 600;
  e->config.rc_buf_optimal_sz = c->rate_buffer_ms < 600 ? c->rate_buffer_ms : 600;
  e->config.rc_buf_sz = c->rate_buffer_ms;
  e->config.rc_dropframe_thresh = 0;
  e->config.rc_resize_allowed = 0;
  if (vpx_codec_enc_init(&e->codec, vpx_codec_vp9_cx(), &e->config, 0) !=
      VPX_CODEC_OK) {
    free(e);
    return NULL;
  }
  if (!control(e, VP8E_SET_CPUUSED, (int)c->cpu_used) ||
      !control(e, VP8E_SET_ENABLEAUTOALTREF, 0) ||
      !control(e, VP8E_SET_STATIC_THRESHOLD, 1) ||
      !control(e, VP8E_SET_MAX_INTRA_BITRATE_PCT, 300) ||
      !control(e, VP9E_SET_AQ_MODE, 3) ||
      !control(e, VP9E_SET_ROW_MT, 1) ||
      !control(e, VP9E_SET_TILE_COLUMNS, log2_floor(c->threads)) ||
      !control(e, VP9E_SET_FRAME_PARALLEL_DECODING, 0) ||
      !control(e, VP9E_SET_NOISE_SENSITIVITY, 0)) {
    meshlive_vpx_close(e);
    return NULL;
  }
  return e;
}

static int collect(MeshVpx *e, MeshVpxSink sink, void *context) {
  vpx_codec_iter_t iterator = NULL;
  const vpx_codec_cx_pkt_t *packet;
  int count = 0;
  while ((packet = vpx_codec_get_cx_data(&e->codec, &iterator)) != NULL) {
    if (packet->kind != VPX_CODEC_CX_FRAME_PKT) continue;
    sink(context, (const uint8_t *)packet->data.frame.buf,
         packet->data.frame.sz, (int64_t)packet->data.frame.pts,
         (packet->data.frame.flags & VPX_FRAME_IS_KEY) != 0);
    count++;
  }
  return count;
}

/* Encodes one NV12 picture (Y plane then interleaved UV, both with stride =
 * width). Returns the number of frames passed to `sink`, or -1. */
int meshlive_vpx_encode(MeshVpx *e, const uint8_t *nv12, size_t size,
                        int64_t pts_us, int force_key, MeshVpxSink sink,
                        void *context) {
  vpx_image_t image;
  if (e == NULL || nv12 == NULL || sink == NULL || pts_us < 0) return -1;
  if (size != (size_t)e->width * e->height * 3 / 2) return -1;
  memset(&image, 0, sizeof(image));
  if (vpx_img_wrap(&image, VPX_IMG_FMT_NV12, e->width, e->height, 1,
                   (unsigned char *)nv12) == NULL)
    return -1;
  if (vpx_codec_encode(&e->codec, &image, pts_us, 1000000 / e->fps,
                       force_key ? VPX_EFLAG_FORCE_KF : 0,
                       VPX_DL_REALTIME) != VPX_CODEC_OK)
    return -1;
  return collect(e, sink, context);
}

int meshlive_vpx_flush(MeshVpx *e, MeshVpxSink sink, void *context) {
  int total = 0;
  int count;
  if (e == NULL || sink == NULL) return -1;
  do {
    if (vpx_codec_encode(&e->codec, NULL, 0, 0, 0, VPX_DL_REALTIME) !=
        VPX_CODEC_OK)
      return -1;
    count = collect(e, sink, context);
    total += count;
  } while (count > 0);
  return total;
}

int meshlive_vpx_set_bitrate(MeshVpx *e, uint32_t bitrate_kbps) {
  if (e == NULL || bitrate_kbps == 0 || bitrate_kbps > 100000) return -1;
  e->config.rc_target_bitrate = bitrate_kbps;
  return vpx_codec_enc_config_set(&e->codec, &e->config) == VPX_CODEC_OK ? 0
                                                                        : -1;
}
