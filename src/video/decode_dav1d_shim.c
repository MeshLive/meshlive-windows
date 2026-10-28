/* MeshLive: a narrow C boundary over the pinned static dav1d 1.5.3 AV1
 * decoder (Design AD, Windows part: the software fallback used only when no
 * Media Foundation AV1 decoder starts). dav1d itself is unmodified; this file
 * only fixes the settings and copies plain values in and out, so the Rust side
 * never mirrors dav1d's structures.
 *
 * Settings (Design AD): n_threads 2, max_frame_delay 1 (no added frame
 * latency), operating_point 0, all_layers 0, strict_std_compliance 1, and a
 * frame_size_limit equal to the session's maximum pixel count.
 *
 * Data ownership: meshlive_dav1d_send copies one temporal unit into a
 * dav1d-owned buffer. When dav1d answers EAGAIN the remainder stays here
 * (dav1d still owns it); the caller drains pictures and then calls
 * meshlive_dav1d_resend until it returns 0. Flush and close release it.
 * SPDX-License-Identifier: GPL-3.0-or-later */
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include "dav1d/dav1d.h"

typedef struct MeshDav1dSequence {
  uint32_t profile;
  uint32_t max_width;
  uint32_t max_height;
  uint32_t layout;       /* 0 I400, 1 I420, 2 I422, 3 I444 */
  uint32_t high_bitdepth;
  uint32_t color_range;  /* 0 limited, 1 full */
  uint32_t matrix;       /* ISO/IEC 23091-4 matrix coefficients */
  uint32_t chroma;       /* 0 unknown, 1 vertical (left), 2 colocated (top-left) */
} MeshDav1dSequence;

typedef struct MeshDav1dPicture {
  const uint8_t *planes[3];
  ptrdiff_t stride_y;
  ptrdiff_t stride_uv;
  uint32_t width;
  uint32_t height;
  uint32_t layout;
  uint32_t bpc;
  int64_t pts;
  MeshDav1dSequence sequence;
} MeshDav1dPicture;

typedef struct MeshDav1d {
  Dav1dContext *context;
  Dav1dData pending;
  Dav1dPicture picture;
  int has_picture;
} MeshDav1d;

const char *meshlive_dav1d_version(void) { return dav1d_version(); }

static void copy_sequence(const Dav1dSequenceHeader *in, MeshDav1dSequence *out) {
  out->profile = (uint32_t)in->profile;
  out->max_width = (uint32_t)in->max_width;
  out->max_height = (uint32_t)in->max_height;
  out->layout = (uint32_t)in->layout;
  out->high_bitdepth = (uint32_t)in->hbd;
  out->color_range = (uint32_t)in->color_range;
  out->matrix = (uint32_t)in->mtrx;
  out->chroma = (uint32_t)in->chr;
}

/* 0: a sequence header was found and copied; 1: none in these bytes;
 * negative: malformed. dav1d's own parser, no MeshLive OBU scanner. */
int meshlive_dav1d_parse_sequence(const uint8_t *data, size_t size, MeshDav1dSequence *out) {
  Dav1dSequenceHeader header;
  int status;
  if (!data || !size || !out) return -EINVAL;
  status = dav1d_parse_sequence_header(&header, data, size);
  if (status == DAV1D_ERR(ENOENT)) return 1;
  if (status < 0) return status;
  copy_sequence(&header, out);
  return 0;
}

MeshDav1d *meshlive_dav1d_open(uint32_t threads, uint32_t frame_delay, uint32_t frame_size_limit) {
  Dav1dSettings settings;
  MeshDav1d *d = (MeshDav1d *)calloc(1, sizeof(MeshDav1d));
  if (!d) return NULL;
  dav1d_default_settings(&settings);
  settings.n_threads = (int)threads;
  settings.max_frame_delay = (int)frame_delay;
  settings.operating_point = 0;
  settings.all_layers = 0;
  settings.strict_std_compliance = 1;
  settings.frame_size_limit = frame_size_limit;
  settings.logger.callback = NULL;
  if (dav1d_open(&d->context, &settings) < 0) {
    free(d);
    return NULL;
  }
  return d;
}

static void release_picture(MeshDav1d *d) {
  if (d->has_picture) {
    dav1d_picture_unref(&d->picture);
    d->has_picture = 0;
  }
}

void meshlive_dav1d_close(MeshDav1d *d) {
  if (!d) return;
  release_picture(d);
  dav1d_data_unref(&d->pending);
  dav1d_close(&d->context);
  free(d);
}

void meshlive_dav1d_flush(MeshDav1d *d) {
  release_picture(d);
  dav1d_data_unref(&d->pending);
  dav1d_flush(d->context);
}

/* 0: all data consumed; 1: EAGAIN, the rest is retained (drain, then resend);
 * negative: a decode error (the caller discards the session). */
int meshlive_dav1d_resend(MeshDav1d *d) {
  int status;
  if (!d->pending.sz) return 0;
  status = dav1d_send_data(d->context, &d->pending);
  if (status == DAV1D_ERR(EAGAIN)) return 1;
  if (status < 0) {
    dav1d_data_unref(&d->pending);
    return status;
  }
  return d->pending.sz ? 1 : 0;
}

int meshlive_dav1d_send(MeshDav1d *d, const uint8_t *data, size_t size, int64_t pts) {
  uint8_t *buffer;
  if (!d || !data || !size) return -EINVAL;
  if (d->pending.sz) return 1; /* the previous unit is not consumed yet */
  buffer = dav1d_data_create(&d->pending, size);
  if (!buffer) return -ENOMEM;
  memcpy(buffer, data, size);
  d->pending.m.timestamp = pts;
  return meshlive_dav1d_resend(d);
}

/* 0: a picture is copied into `out` and stays valid until the next get,
 * release, flush or close; 1: none yet (EAGAIN); negative: error. */
int meshlive_dav1d_get(MeshDav1d *d, MeshDav1dPicture *out) {
  int status;
  release_picture(d);
  status = dav1d_get_picture(d->context, &d->picture);
  if (status == DAV1D_ERR(EAGAIN)) return 1;
  if (status < 0) return status;
  d->has_picture = 1;
  out->planes[0] = (const uint8_t *)d->picture.data[0];
  out->planes[1] = (const uint8_t *)d->picture.data[1];
  out->planes[2] = (const uint8_t *)d->picture.data[2];
  out->stride_y = d->picture.stride[0];
  out->stride_uv = d->picture.stride[1];
  out->width = (uint32_t)d->picture.p.w;
  out->height = (uint32_t)d->picture.p.h;
  out->layout = (uint32_t)d->picture.p.layout;
  out->bpc = (uint32_t)d->picture.p.bpc;
  out->pts = d->picture.m.timestamp;
  copy_sequence(d->picture.seq_hdr, &out->sequence);
  return 0;
}

void meshlive_dav1d_release(MeshDav1d *d) { release_picture(d); }
