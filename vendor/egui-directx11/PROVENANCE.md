# Renderer source provenance

Upstream egui-directx11 0.12.1 from https://static.crates.io/crates/egui-directx11/egui-directx11-0.12.1.crate . SHA256 a1a3fea83a5effd3fa95902753dd54021c263e49af39953424e7b25f84040cb6 verified before extraction. MIT option selected; both upstream license texts retained.

Retained files: src/lib.rs, src/texture.rs, shaders/egui.hlsl and licenses. No upstream example, metadata author email or bundled shader binary is included. Local modifications: minimal manifest pins egui 0.33.3/windows 0.62.2 and disables default fonts; build.rs compiles original HLSL with installed fxc as vs_4_0/ps_4_0 for D3D feature level 10_0; include_bytes paths load those generated files. No custom cryptography or renderer redesign.

The upstream renderer creates per-mesh buffers and converts vertices. Measure this before optimizing it. Current user textures are RGBA shader-resource views; NV12 decoder-plane presentation and actual Media Foundation decode are not implemented by this renderer alone.

Local zoom correction: egui FullOutput.pixels_per_point already includes both user zoom and native display scaling. Remove the extra Context.zoom_factor multiplier from vertex positions and clip rectangles. The native WARP pixel regression fails at 2x with the original code and checks geometry/clipping at 1x, 2x and combined 1.5x zoom/1.5x native DPI. Headless full-page renders are optional test artifacts, not desktop captures.

The two unchanged upstream source files retain their original CRLF bytes through explicit -text Git attributes, so their frozen hashes also match a fresh checkout. Verify both working bytes and staged blobs with scripts/check_sources.py and --index. Modified renderer glue follows normal repository line-ending rules.
