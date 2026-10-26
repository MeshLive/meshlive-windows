/* MeshLive: a narrow C boundary over the pinned static libaom AV1 encoder, so
 * Rust never mirrors libaom's version-specific struct layouts. Settings follow
 * the Android app's native_aom_encoder.cpp default arm (NativeAomTuning.DEFAULT):
 * one-pass real-time usage, CBR, lag 0, no automatic keyframes (the caller
 * forces them on request and as the 60 s safety net), cyclic-refresh AQ,
 * PSNR tuning, CDEF on, 3 reference frames, the WebRTC real-time tool profile,
 * row MT and auto tiles. Frame dropping is off (Android's default arm), so
 * every input frame produces exactly one temporal unit.
 * SPDX-License-Identifier: GPL-3.0-or-later */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include "aom/aom_encoder.h"
#include "aom/aomcx.h"

typedef struct MeshAomConfig {
  uint32_t width;
  uint32_t height;
  uint32_t fps;
  uint32_t bitrate_kbps;
  uint32_t cpu_used;
  uint32_t threads;
  uint32_t rate_buffer_ms;
} MeshAomConfig;

typedef void (*MeshAomSink)(void *context, const uint8_t *data, size_t size,
                            int64_t pts_us, int key_frame);

typedef struct MeshAom {
  aom_codec_ctx_t codec;
  uint32_t width;
  uint32_t height;
  uint32_t fps;
} MeshAom;

static int control(MeshAom *e, int id, int value) {
  return aom_codec_control(&e->codec, id, value) == AOM_CODEC_OK;
}

static int realtime_tools(MeshAom *e) {
  return control(e, AV1E_SET_ENABLE_TPL_MODEL, 0) &&
         control(e, AV1E_SET_DELTAQ_MODE, 0) &&
         control(e, AV1E_SET_ENABLE_ORDER_HINT, 0) &&
         control(e, AV1E_SET_COEFF_COST_UPD_FREQ, 3) &&
         control(e, AV1E_SET_MODE_COST_UPD_FREQ, 3) &&
         control(e, AV1E_SET_MV_COST_UPD_FREQ, 3) &&
         control(e, AV1E_SET_ENABLE_PALETTE, 0) &&
         control(e, AV1E_SET_ENABLE_OBMC, 0) &&
         control(e, AV1E_SET_ENABLE_WARPED_MOTION, 0) &&
         control(e, AV1E_SET_ENABLE_GLOBAL_MOTION, 0) &&
         control(e, AV1E_SET_ENABLE_REF_FRAME_MVS, 0) &&
         control(e, AV1E_SET_SUPERBLOCK_SIZE, AOM_SUPERBLOCK_SIZE_DYNAMIC) &&
         control(e, AV1E_SET_ENABLE_CFL_INTRA, 0) &&
         control(e, AV1E_SET_ENABLE_SMOOTH_INTRA, 0) &&
         control(e, AV1E_SET_ENABLE_ANGLE_DELTA, 0) &&
         control(e, AV1E_SET_ENABLE_FILTER_INTRA, 0) &&
         control(e, AV1E_SET_INTRA_DEFAULT_TX_ONLY, 1) &&
         control(e, AV1E_SET_DISABLE_TRELLIS_QUANT, 1) &&
         control(e, AV1E_SET_ENABLE_DIST_WTD_COMP, 0) &&
         control(e, AV1E_SET_ENABLE_DIFF_WTD_COMP, 0) &&
         control(e, AV1E_SET_ENABLE_DUAL_FILTER, 0) &&
         control(e, AV1E_SET_ENABLE_INTERINTRA_COMP, 0) &&
         control(e, AV1E_SET_ENABLE_INTERINTRA_WEDGE, 0) &&
         control(e, AV1E_SET_ENABLE_INTRA_EDGE_FILTER, 0) &&
         control(e, AV1E_SET_ENABLE_INTRABC, 0) &&
         control(e, AV1E_SET_ENABLE_MASKED_COMP, 0) &&
         control(e, AV1E_SET_ENABLE_PAETH_INTRA, 0) &&
         control(e, AV1E_SET_ENABLE_QM, 0) &&
         control(e, AV1E_SET_ENABLE_RECT_PARTITIONS, 0) &&
         control(e, AV1E_SET_ENABLE_RESTORATION, 0) &&
         control(e, AV1E_SET_ENABLE_SMOOTH_INTERINTRA, 0) &&
         control(e, AV1E_SET_ENABLE_TX64, 0);
}

const char *meshlive_aom_version(void) { return aom_codec_version_str(); }

void meshlive_aom_close(MeshAom *e) {
  if (e == NULL) return;
  aom_codec_destroy(&e->codec);
  free(e);
}

MeshAom *meshlive_aom_open(const MeshAomConfig *c) {
  aom_codec_enc_cfg_t cfg;
  MeshAom *e;
  if (c == NULL || c->width == 0 || c->height == 0 || (c->width & 1) ||
      (c->height & 1) || c->width > 4096 || c->height > 4096 || c->fps == 0 ||
      c->fps > 120 || c->bitrate_kbps == 0 || c->bitrate_kbps > 100000 ||
      c->cpu_used < 5 || c->cpu_used > 10 || c->threads == 0 ||
      c->threads > 16 || c->rate_buffer_ms < 100 || c->rate_buffer_ms > 6000)
    return NULL;
  if (aom_codec_enc_config_default(aom_codec_av1_cx(), &cfg,
                                   AOM_USAGE_REALTIME) != AOM_CODEC_OK)
    return NULL;
  cfg.g_w = c->width;
  cfg.g_h = c->height;
  cfg.g_threads = c->threads;
  cfg.g_timebase.num = 1;
  cfg.g_timebase.den = 1000000;
  cfg.g_input_bit_depth = 8;
  cfg.g_profile = 0;
  cfg.g_pass = AOM_RC_ONE_PASS;
  cfg.g_lag_in_frames = 0;
  cfg.g_error_resilient = 0;
  cfg.kf_mode = AOM_KF_DISABLED;
  cfg.rc_end_usage = AOM_CBR;
  cfg.rc_target_bitrate = c->bitrate_kbps;
  cfg.rc_min_quantizer = 10;
  cfg.rc_max_quantizer = 63;
  cfg.rc_undershoot_pct = 50;
  cfg.rc_overshoot_pct = 50;
  cfg.rc_buf_initial_sz = c->rate_buffer_ms < 600 ? c->rate_buffer_ms : 600;
  cfg.rc_buf_optimal_sz = c->rate_buffer_ms < 600 ? c->rate_buffer_ms : 600;
  cfg.rc_buf_sz = c->rate_buffer_ms;
  cfg.rc_dropframe_thresh = 0;
  e = (MeshAom *)calloc(1, sizeof(MeshAom));
  if (e == NULL) return NULL;
  e->width = c->width;
  e->height = c->height;
  e->fps = c->fps;
  if (aom_codec_enc_init(&e->codec, aom_codec_av1_cx(), &cfg, 0) !=
      AOM_CODEC_OK) {
    free(e);
    return NULL;
  }
  if (!control(e, AOME_SET_CPUUSED, (int)c->cpu_used) ||
      !control(e, AV1E_SET_ENABLE_CDEF, 1) ||
      !control(e, AV1E_SET_AQ_MODE, 3) ||
      !control(e, AOME_SET_TUNING, AOM_TUNE_PSNR) ||
      !control(e, AV1E_SET_NOISE_SENSITIVITY, 0) ||
      !control(e, AOME_SET_MAX_INTRA_BITRATE_PCT, 300) ||
      !control(e, AV1E_SET_AUTO_TILES, 1) || !control(e, AV1E_SET_ROW_MT, 1) ||
      !control(e, AV1E_SET_MAX_REFERENCE_FRAMES, 3) || !realtime_tools(e)) {
    meshlive_aom_close(e);
    return NULL;
  }
  return e;
}

static int collect(MeshAom *e, MeshAomSink sink, void *context) {
  aom_codec_iter_t iterator = NULL;
  const aom_codec_cx_pkt_t *packet;
  int count = 0;
  while ((packet = aom_codec_get_cx_data(&e->codec, &iterator)) != NULL) {
    if (packet->kind != AOM_CODEC_CX_FRAME_PKT) continue;
    sink(context, (const uint8_t *)packet->data.frame.buf,
         packet->data.frame.sz, (int64_t)packet->data.frame.pts,
         (packet->data.frame.flags & AOM_FRAME_IS_KEY) != 0);
    count++;
  }
  return count;
}

/* Encodes one NV12 picture (Y plane then interleaved UV, both with stride =
 * width). Returns the number of temporal units passed to `sink`, or -1. */
int meshlive_aom_encode(MeshAom *e, const uint8_t *nv12, size_t size,
                        int64_t pts_us, int force_key, MeshAomSink sink,
                        void *context) {
  aom_image_t image;
  size_t needed;
  if (e == NULL || nv12 == NULL || sink == NULL || pts_us < 0) return -1;
  needed = (size_t)e->width * e->height * 3 / 2;
  if (size != needed) return -1;
  memset(&image, 0, sizeof(image));
  if (aom_img_wrap(&image, AOM_IMG_FMT_NV12, e->width, e->height, 1,
                   (unsigned char *)nv12) == NULL)
    return -1;
  if (aom_codec_encode(&e->codec, &image, pts_us, 1000000 / e->fps,
                       force_key ? AOM_EFLAG_FORCE_KF : 0) != AOM_CODEC_OK)
    return -1;
  return collect(e, sink, context);
}

/* End of input: passes any remaining units to `sink`. */
int meshlive_aom_flush(MeshAom *e, MeshAomSink sink, void *context) {
  int total = 0;
  int count;
  if (e == NULL || sink == NULL) return -1;
  do {
    if (aom_codec_encode(&e->codec, NULL, 0, 0, 0) != AOM_CODEC_OK) return -1;
    count = collect(e, sink, context);
    total += count;
  } while (count > 0);
  return total;
}

/* Rate change without a restart (the Android adjuster's effective rate). */
int meshlive_aom_set_bitrate(MeshAom *e, uint32_t bitrate_kbps) {
  aom_codec_enc_cfg_t cfg;
  if (e == NULL || bitrate_kbps == 0 || bitrate_kbps > 100000) return -1;
  cfg = *e->codec.config.enc;
  cfg.rc_target_bitrate = bitrate_kbps;
  return aom_codec_enc_config_set(&e->codec, &cfg) == AOM_CODEC_OK ? 0 : -1;
}
