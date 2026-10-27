//! Feasibility probe: does each GPU expose HEVC Main decoding through D3D11
//! video decoding (DXVA), independent of any Media Foundation HEVC decoder?
//! Enumeration plus one decoder object creation per adapter; no bitstream is
//! decoded, no MF runtime is started in the default mode, no capture/output/radio.
//!
//!   cargo run --example hevc_d3d11_probe            D3D11 video decoder inventory
//!   cargo run --example hevc_d3d11_probe -- --mf    also count MF HEVC decoder MFTs
//!
//! The D3D11 inventory runs first and lists every loaded module afterwards, so
//! the output itself shows whether anything from a Store codec package (the
//! "HEVC Video Extensions") was involved in the answer.
use windows::{
    Win32::{
        Foundation::{HMODULE, MAX_PATH},
        Graphics::{
            Direct3D::*,
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
        Media::MediaFoundation::*,
        System::{
            Com::*, LibraryLoader::GetModuleFileNameW, ProcessStatus::EnumProcessModules,
            Threading::GetCurrentProcess,
        },
    },
    core::{GUID, Interface},
};

/// DXVA_NoEncrypt (dxva.h): the clear-bitstream configuration.
const DXVA_NO_ENCRYPT: GUID = GUID::from_u128(0x1b81bed0_a0c7_11d3_b984_00c04f2e73c5);

fn profile_name(guid: &GUID) -> String {
    let known: &[(GUID, &str)] = &[
        (D3D11_DECODER_PROFILE_HEVC_VLD_MAIN, "HEVC_VLD_MAIN"),
        (D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10, "HEVC_VLD_MAIN10"),
        (D3D11_DECODER_PROFILE_H264_VLD_NOFGT, "H264_VLD_NOFGT"),
        (D3D11_DECODER_PROFILE_H264_VLD_FGT, "H264_VLD_FGT"),
        (
            D3D11_DECODER_PROFILE_H264_VLD_STEREO_NOFGT,
            "H264_VLD_STEREO_NOFGT",
        ),
        (
            D3D11_DECODER_PROFILE_H264_VLD_STEREO_PROGRESSIVE_NOFGT,
            "H264_VLD_STEREO_PROGRESSIVE_NOFGT",
        ),
        (
            D3D11_DECODER_PROFILE_H264_VLD_MULTIVIEW_NOFGT,
            "H264_VLD_MULTIVIEW_NOFGT",
        ),
        (D3D11_DECODER_PROFILE_VP9_VLD_PROFILE0, "VP9_VLD_PROFILE0"),
        (
            D3D11_DECODER_PROFILE_VP9_VLD_10BIT_PROFILE2,
            "VP9_VLD_10BIT_PROFILE2",
        ),
        (D3D11_DECODER_PROFILE_VP8_VLD, "VP8_VLD"),
        (D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0, "AV1_VLD_PROFILE0"),
        (D3D11_DECODER_PROFILE_AV1_VLD_PROFILE1, "AV1_VLD_PROFILE1"),
        (D3D11_DECODER_PROFILE_AV1_VLD_PROFILE2, "AV1_VLD_PROFILE2"),
        (D3D11_DECODER_PROFILE_MPEG2_VLD, "MPEG2_VLD"),
        (D3D11_DECODER_PROFILE_MPEG2and1_VLD, "MPEG2and1_VLD"),
        (D3D11_DECODER_PROFILE_MPEG1_VLD, "MPEG1_VLD"),
        (D3D11_DECODER_PROFILE_VC1_VLD, "VC1_VLD"),
        (D3D11_DECODER_PROFILE_VC1_D2010, "VC1_D2010"),
        (
            D3D11_DECODER_PROFILE_MPEG4PT2_VLD_SIMPLE,
            "MPEG4PT2_VLD_SIMPLE",
        ),
        (
            D3D11_DECODER_PROFILE_MPEG4PT2_VLD_ADVSIMPLE_NOGMC,
            "MPEG4PT2_VLD_ADVSIMPLE_NOGMC",
        ),
    ];
    known
        .iter()
        .find(|(g, _)| g == guid)
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| format!("{guid:?} (vendor/other)"))
}

fn wide(text: &[u16]) -> String {
    let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

/// Largest of the listed sizes for which the driver offers at least one config.
fn configs(video: &ID3D11VideoDevice, profile: GUID, format: DXGI_FORMAT) -> Vec<(u32, u32, u32)> {
    let sizes = [
        (1280, 720),
        (1920, 1080),
        (2560, 1440),
        (3840, 2160),
        (4096, 2304),
        (7680, 4320),
        (8192, 8192),
    ];
    sizes
        .iter()
        .filter_map(|&(w, h)| {
            let desc = D3D11_VIDEO_DECODER_DESC {
                Guid: profile,
                SampleWidth: w,
                SampleHeight: h,
                OutputFormat: format,
            };
            let count = unsafe { video.GetVideoDecoderConfigCount(&desc) }.unwrap_or(0);
            (count > 0).then_some((w, h, count))
        })
        .collect()
}

fn probe_adapter(adapter: &IDXGIAdapter1) -> windows::core::Result<()> {
    let desc = unsafe { adapter.GetDesc1()? };
    let software = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
    println!(
        "\nAdapter: {} (vendor {:#06x}, device {:#06x}{})",
        wide(&desc.Description),
        desc.VendorId,
        desc.DeviceId,
        if software { ", software" } else { "" }
    );
    let mut gpu = None;
    let mut level = D3D_FEATURE_LEVEL_10_0;
    let created = unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0,
            ]),
            D3D11_SDK_VERSION,
            Some(&mut gpu),
            Some(&mut level),
            None,
        )
    };
    if let Err(e) = created {
        println!("  D3D11 device with VIDEO_SUPPORT: unavailable ({e})");
        return Ok(());
    }
    let gpu = gpu.unwrap();
    let video: ID3D11VideoDevice = gpu.cast()?;
    let count = unsafe { video.GetVideoDecoderProfileCount() };
    println!("  feature level {:#x}; {count} decoder profiles:", level.0);
    let mut profiles = Vec::new();
    for i in 0..count {
        let guid = unsafe { video.GetVideoDecoderProfile(i)? };
        println!("    {}", profile_name(&guid));
        profiles.push(guid);
    }
    for (profile, format, label) in [
        (
            D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
            DXGI_FORMAT_NV12,
            "HEVC Main -> NV12",
        ),
        (
            D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10,
            DXGI_FORMAT_P010,
            "HEVC Main10 -> P010",
        ),
    ] {
        let listed = profiles.contains(&profile);
        let format_ok = listed
            && unsafe { video.CheckVideoDecoderFormat(&profile, format) }
                .map(|b| b.as_bool())
                .unwrap_or(false);
        let sizes = if format_ok {
            configs(&video, profile, format)
        } else {
            Vec::new()
        };
        println!(
            "  {label}: profile listed={listed}, output format ok={format_ok}, sizes with configs={}",
            if sizes.is_empty() {
                "none".to_string()
            } else {
                sizes
                    .iter()
                    .map(|(w, h, n)| format!("{w}x{h}({n})"))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        );
        if !format_ok || sizes.is_empty() {
            continue;
        }
        // Create (and drop) one real decoder object at 1920x1080 or the first
        // supported size: proof the driver instantiates it, not just lists it.
        let (w, h, n) = sizes
            .iter()
            .copied()
            .find(|&(w, _, _)| w == 1920)
            .unwrap_or(sizes[0]);
        let desc = D3D11_VIDEO_DECODER_DESC {
            Guid: profile,
            SampleWidth: w,
            SampleHeight: h,
            OutputFormat: format,
        };
        for index in 0..n {
            let mut config = D3D11_VIDEO_DECODER_CONFIG::default();
            unsafe { video.GetVideoDecoderConfig(&desc, index, &mut config)? };
            println!(
                "    config {index} @{w}x{h}: ConfigBitstreamRaw={} encryption={} min render targets={}",
                config.ConfigBitstreamRaw,
                if config.guidConfigBitstreamEncryption == DXVA_NO_ENCRYPT {
                    "none".to_string()
                } else {
                    format!("{:?}", config.guidConfigBitstreamEncryption)
                },
                config.ConfigMinRenderTargetBuffCount
            );
            if index == 0 {
                match unsafe { video.CreateVideoDecoder(&desc, &config) } {
                    Ok(decoder) => {
                        let mut got = D3D11_VIDEO_DECODER_DESC::default();
                        let mut got_config = D3D11_VIDEO_DECODER_CONFIG::default();
                        unsafe { decoder.GetCreationParameters(&mut got, &mut got_config)? };
                        println!(
                            "    CreateVideoDecoder: OK ({}x{}, ConfigBitstreamRaw={})",
                            got.SampleWidth, got.SampleHeight, got_config.ConfigBitstreamRaw
                        );
                    }
                    Err(e) => println!("    CreateVideoDecoder: FAILED ({e})"),
                }
            }
        }
    }
    Ok(())
}

fn loaded_modules() -> Vec<String> {
    let mut modules = vec![HMODULE::default(); 1024];
    let mut needed = 0u32;
    let process = unsafe { GetCurrentProcess() };
    if unsafe {
        EnumProcessModules(
            process,
            modules.as_mut_ptr(),
            (modules.len() * std::mem::size_of::<HMODULE>()) as u32,
            &mut needed,
        )
    }
    .is_err()
    {
        return Vec::new();
    }
    let n = (needed as usize / std::mem::size_of::<HMODULE>()).min(modules.len());
    modules[..n]
        .iter()
        .map(|&m| {
            let mut buffer = [0u16; MAX_PATH as usize * 2];
            let len = unsafe { GetModuleFileNameW(Some(m), &mut buffer) } as usize;
            String::from_utf16_lossy(&buffer[..len])
        })
        .collect()
}

fn mf_hevc_count(flags: MFT_ENUM_FLAG) -> windows::core::Result<u32> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_HEVC,
    };
    let mut pointer: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            flags,
            Some(&input),
            None,
            &mut pointer,
            &mut count,
        )?;
        if !pointer.is_null() {
            std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(pointer, count as usize));
            CoTaskMemFree(Some(pointer.cast()));
        }
    }
    Ok(count)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let with_mf = args.iter().any(|a| a == "--mf");
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut index = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(index) } {
        probe_adapter(&adapter)?;
        index += 1;
    }
    let modules = loaded_modules();
    // modules[0] is this executable (its own name contains "hevc").
    let suspicious: Vec<&String> = modules
        .iter()
        .skip(1)
        .filter(|m| {
            let l = m.to_ascii_lowercase();
            l.contains("windowsapps") || l.contains("hevc")
        })
        .collect();
    println!(
        "\nModules loaded by the D3D11 path: {} total; modules from a Store package or named HEVC: {}",
        modules.len(),
        if suspicious.is_empty() {
            "none".to_string()
        } else {
            suspicious
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    if std::env::var_os("HEVC_PROBE_MODULES").is_some() {
        for m in &modules {
            println!("    {m}");
        }
    }
    if with_mf {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)?;
        }
        let hardware = mf_hevc_count(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER)?;
        let software = mf_hevc_count(
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
        )?;
        println!(
            "\nFor comparison, MF HEVC decoder MFTs (enumeration only): hardware={hardware}, software={software}"
        );
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
    println!("No bitstream decoded; no camera, microphone, output or radio opened.");
    Ok(())
}
