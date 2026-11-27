//! Synthetic streamed textures and layered offscreen rasterisation.
//! Run the ignored `streaming_render_soak` test in a release build.
//! Set `VICTORY_SOAK_SECONDS` for duration, `VICTORY_SOAK_ALIASES` to
//! `direct` or `through` for alias copies, or `VICTORY_SOAK_GPU_UPLOAD=1`
//! to measure a mapped VRAM upload buffer.

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::{Interface, s};

use crate::host::{compile, host, transition};
use crate::texture::AliasCopy;

const TILE: u32 = 256;
const WIDTH: u32 = TILE * 4;
const HEIGHT: u32 = TILE * 3;
const VIEWS: u32 = 12;
const RESIDENT: u32 = 96;
const SIDE: u32 = 512;
const UPLOAD: u64 = SIDE as u64 * SIDE as u64 * 4;
const FRAME: Duration = Duration::from_nanos(1_000_000_000 / 60);

const DRAW: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility=SHADER_VISIBILITY_PIXEL)"
Texture2D<float4> material : register(t0);
[RootSignature(RS)]
float4 vertex(uint i : SV_VertexID) : SV_Position {
    float2 p = float2((i << 1) & 2, i & 2);
    return float4(p * 2 - 1, 0, 1);
}
[RootSignature(RS)]
float4 pixel(float4 at : SV_Position) : SV_Target {
    int2 p = (int2(at.xy) * 2) & 511;
    float3 a = material.Load(int3(p, 0)).rgb;
    float3 b = material.Load(int3((p + int2(13, 19)) & 511, 0)).rgb;
    float3 c = material.Load(int3((p + int2(29, 43)) & 511, 0)).rgb;
    return float4(a * 0.5 + b * 0.25 + c * 0.25, 1);
}
"#;

fn image(width: u32, height: u32, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(width),
        Height: height,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Flags: flags,
        ..Default::default()
    }
}

fn subresource(resource: &ID3D12Resource) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: the caller keeps the resource alive through execution.
        pResource: unsafe { std::mem::transmute_copy(resource) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        ..Default::default()
    }
}

fn footprint(
    resource: &ID3D12Resource,
    offset: u64,
    [width, height, pitch]: [u32; 3],
) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: the caller keeps the resource alive through execution.
        pResource: unsafe { std::mem::transmute_copy(resource) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: offset,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    Width: width,
                    Height: height,
                    Depth: 1,
                    RowPitch: pitch,
                },
            },
        },
    }
}

fn private_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut counter = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: a writable counter of the indicated size for our process.
    assert_ne!(
        unsafe {
            K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut counter).cast(), counter.cb)
        },
        0
    );
    counter.PrivateUsage as u64
}

fn percentiles(times: &[f64]) -> [f64; 3] {
    let mut times = times.to_vec();
    times.sort_unstable_by(f64::total_cmp);
    [50, 95, 99].map(|p| times[(times.len() * p).div_ceil(100) - 1])
}

#[test]
#[ignore = "sustained D3D12 hardware benchmark, VICTORY_SOAK_SECONDS defaults to 90"]
fn streaming_render_soak() {
    let seconds = std::env::var("VICTORY_SOAK_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(90u64);
    let aliases = std::env::var("VICTORY_SOAK_ALIASES").unwrap_or_default();
    assert!(matches!(aliases.as_str(), "" | "direct" | "through"));
    let host = host().expect("D3D12 device");
    let gpu_upload = std::env::var("VICTORY_SOAK_GPU_UPLOAD").is_ok_and(|value| value == "1");
    println!("GPU_UPLOAD supported={}", host.mapped_card_memory());
    let mapped = gpu_upload.then(|| {
        host.mapped_card_buffer(UPLOAD * u64::from(VIEWS), false)
            .expect("GPU_UPLOAD buffer")
    });
    let device = &host.device;
    let vs = compile(DRAW, s!("vertex"), s!("vs_5_1")).unwrap();
    let ps = compile(DRAW, s!("pixel"), s!("ps_5_1")).unwrap();
    // SAFETY: valid D3D12 descriptions; objects, upload pieces and target mappings
    // outlive the fence for each recorded frame.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &vs).unwrap();
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        };
        let mut blend = D3D12_BLEND_DESC::default();
        blend.RenderTarget[0].RenderTargetWriteMask = 15;
        let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
        formats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        let pso: ID3D12PipelineState = device
            .CreateGraphicsPipelineState(&D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: code(&vs),
                PS: code(&ps),
                BlendState: blend,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                SampleMask: u32::MAX,
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: 1,
                RTVFormats: formats,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            })
            .unwrap();

        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC::default())
            .unwrap();
        let frequency = queue.GetTimestampFrequency().unwrap() as f64;
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, &pso)
            .unwrap();
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        let mut stamps: Option<ID3D12QueryHeap> = None;
        device
            .CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: 2,
                    NodeMask: 0,
                },
                &mut stamps,
            )
            .unwrap();
        let stamps = stamps.unwrap();
        let out = host
            .read_back_buffer(512 + u64::from(WIDTH * HEIGHT * 4))
            .unwrap();

        let descriptor = |ty, count, flags| -> ID3D12DescriptorHeap {
            device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: ty,
                    NumDescriptors: count,
                    Flags: flags,
                    NodeMask: 0,
                })
                .unwrap()
        };
        let srv_heap = descriptor(
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            RESIDENT,
            D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
        );
        let srv_cpu = srv_heap.GetCPUDescriptorHandleForHeapStart().ptr;
        let srv_gpu = srv_heap.GetGPUDescriptorHandleForHeapStart().ptr;
        let stride =
            device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
        let mut materials = Vec::new();
        for i in 0..RESIDENT {
            let tex = super::texture(
                device,
                &image(SIDE, SIDE, D3D12_RESOURCE_FLAG_NONE),
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            )
            .unwrap();
            device.CreateShaderResourceView(
                &tex,
                None,
                D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: srv_cpu + (i * stride) as usize,
                },
            );
            materials.push(tex);
        }
        let mut sources = Vec::new();
        let mut copies = Vec::new();
        if !aliases.is_empty() {
            let desc = D3D12_RESOURCE_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UINT,
                ..image(SIDE, SIDE, D3D12_RESOURCE_FLAG_NONE)
            };
            for _ in 0..VIEWS {
                sources
                    .push(super::texture(device, &desc, D3D12_RESOURCE_STATE_COPY_SOURCE).unwrap());
            }
            for (i, target) in materials.iter().enumerate() {
                let source = &sources[i % VIEWS as usize];
                let copy = match aliases.as_str() {
                    "direct" => AliasCopy::direct(source, target),
                    "through" => AliasCopy::through(host, source, target),
                    _ => unreachable!(),
                };
                copies.push(copy.expect("compatible alias layout"));
            }
            let bytes: u64 = copies
                .iter()
                .filter_map(AliasCopy::buffer)
                .map(|buffer| {
                    device
                        .GetResourceAllocationInfo(0, &[buffer.GetDesc()])
                        .SizeInBytes
                })
                .sum();
            println!("alias buffers={bytes}");
        }
        let targets = descriptor(
            D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            1,
            D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
        );
        let rtv = targets.GetCPUDescriptorHandleForHeapStart();
        let mut image_out: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &image(WIDTH, HEIGHT, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET),
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                None,
                &mut image_out,
            )
            .unwrap();
        let image_out = image_out.unwrap();
        device.CreateRenderTargetView(&image_out, None, rtv);
        let copy_out = footprint(&out, 512, [WIDTH, HEIGHT, WIDTH * 4]);
        let mut mapping = std::ptr::null_mut();
        out.Map(0, None, Some(&mut mapping)).unwrap();
        let target_data = mapping.cast::<u8>().add(512);

        println!("frame,time_ms,gpu_ms,private_mib,local_mib,nonlocal_mib,staging_mib,texture_mib");
        let begin = Instant::now();
        let mut times = Vec::new();
        let mut gpu_times = Vec::new();
        let mut frame = 0u64;
        let mut peaks = [0u64; 5];
        while begin.elapsed() < Duration::from_secs(seconds) {
            let start = Instant::now();
            let first = ((frame * u64::from(VIEWS)) % u64::from(RESIDENT)) as u32;
            let value = |i: u32| ((frame.wrapping_mul(13) + u64::from(i) * 11) % 251) as u8;
            let mut staging = Vec::with_capacity(VIEWS as usize);
            list.EndQuery(&stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0);
            for i in 0..VIEWS {
                let tex = &materials[(first + i) as usize];
                let source = sources.get(i as usize);
                let (upload_target, state) = source.map_or(
                    (tex, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE),
                    |source| (source, D3D12_RESOURCE_STATE_COPY_SOURCE),
                );
                let (buffer, offset, data) = if let Some((buffer, data)) = &mapped {
                    let offset = UPLOAD * u64::from(i);
                    (buffer, offset, data.add(offset as usize))
                } else {
                    staging.push(super::staging(host, UPLOAD).expect("staging allocation"));
                    let (_, buffer, offset, data) = staging.last().unwrap();
                    (buffer, *offset, *data)
                };
                std::ptr::write_bytes(data, value(i), UPLOAD as usize);
                list.ResourceBarrier(&[transition(
                    upload_target,
                    state,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )]);
                let mut layout = footprint(buffer, offset, [SIDE, SIDE, SIDE * 4]);
                if let Some(source) = source {
                    layout.Anonymous.PlacedFootprint.Footprint.Format = source.GetDesc().Format;
                }
                list.CopyTextureRegion(&subresource(upload_target), 0, 0, 0, &layout, None);
                if let Some(source) = source {
                    list.ResourceBarrier(&[
                        transition(
                            source,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            D3D12_RESOURCE_STATE_COPY_SOURCE,
                        ),
                        transition(
                            tex,
                            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                        ),
                    ]);
                    copies[(first + i) as usize].record(&list, source, tex);
                }
                list.ResourceBarrier(&[transition(
                    tex,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                )]);
            }
            list.SetGraphicsRootSignature(&root);
            list.SetDescriptorHeaps(&[Some(srv_heap.clone())]);
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            list.OMSetRenderTargets(1, Some(&rtv), false, None);
            list.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: WIDTH as i32,
                bottom: HEIGHT as i32,
            }]);
            for _ in 0..4 {
                for i in 0..VIEWS {
                    list.RSSetViewports(&[D3D12_VIEWPORT {
                        TopLeftX: (i % 4 * TILE) as f32,
                        TopLeftY: (i / 4 * TILE) as f32,
                        Width: TILE as f32,
                        Height: TILE as f32,
                        MinDepth: 0.0,
                        MaxDepth: 1.0,
                    }]);
                    list.SetGraphicsRootDescriptorTable(
                        0,
                        D3D12_GPU_DESCRIPTOR_HANDLE {
                            ptr: srv_gpu + u64::from((first + i) * stride),
                        },
                    );
                    list.DrawInstanced(3, 1, 0, 0);
                }
            }
            list.EndQuery(&stamps, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            list.ResolveQueryData(&stamps, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &out, 0);
            let verify = frame.is_multiple_of(30);
            if verify {
                list.ResourceBarrier(&[transition(
                    &image_out,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                list.CopyTextureRegion(&copy_out, 0, 0, 0, &subresource(&image_out), None);
                list.ResourceBarrier(&[transition(
                    &image_out,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                )]);
            }
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            queue.Signal(&fence, frame + 1).unwrap();
            fence.SetEventOnCompletion(frame + 1, event).unwrap();
            assert_eq!(WaitForSingleObject(event, 10_000), WAIT_OBJECT_0);
            let wall = start.elapsed().as_secs_f64() * 1_000.0;
            let times_gpu = mapping.cast::<u64>();
            let gpu = (times_gpu.add(1).read_volatile() - times_gpu.read_volatile()) as f64
                * 1_000.0
                / frequency;
            allocator.Reset().unwrap();
            list.Reset(&allocator, &pso).unwrap();
            drop(staging);

            if verify {
                for y in 0..HEIGHT {
                    for x in 0..WIDTH {
                        let i = y / TILE * 4 + x / TILE;
                        let got = target_data
                            .cast::<[u8; 4]>()
                            .add((y * WIDTH + x) as usize)
                            .read();
                        assert_eq!(
                            got,
                            [value(i), value(i), value(i), 255],
                            "frame {frame}, pixel ({x}, {y})"
                        );
                    }
                }
            }
            if begin.elapsed().as_secs() >= 5 {
                times.push(wall);
                gpu_times.push(gpu);
            }
            if frame.is_multiple_of(15) {
                let memory = host.video_memory().unwrap();
                let [_, local, _, nonlocal] = memory;
                if frame.is_multiple_of(60) {
                    crate::residency::refresh();
                    super::room(Some(memory));
                }
                let stage = super::lock(&super::STAGING).blocks() as u64 * super::BLOCK;
                let textures = super::lock(&super::TEXTURES).blocks() as u64 * super::BLOCK;
                let sizes = [private_bytes(), local, nonlocal, stage, textures].map(|v| v >> 20);
                for (peak, size) in peaks.iter_mut().zip(sizes) {
                    *peak = (*peak).max(size);
                }
                let [private, local, nonlocal, stage, textures] = sizes;
                println!(
                    "{frame},{wall:.3},{gpu:.3},{private},{local},{nonlocal},{stage},{textures}"
                );
                assert_eq!(host.log_messages(), None);
            }
            frame += 1;
            std::thread::sleep(FRAME.saturating_sub(start.elapsed()));
        }
        let third = times.len() / 3;
        assert!(third > 0);
        println!(
            "summary frames={frame} wall={:?} gpu={:?} peaks={peaks:?} first={:?} last={:?}",
            percentiles(&times),
            percentiles(&gpu_times),
            percentiles(&times[..third]),
            percentiles(&times[times.len() - third..])
        );
        list.Close().unwrap();
        out.Unmap(0, None);
        CloseHandle(event).unwrap();
    }
}
