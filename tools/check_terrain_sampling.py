"""GPU regression for tile-mask/light-map edges (requires Python package wgpu).

Run: python tools/check_terrain_sampling.py
Compiles the actual combined scene shader, then renders its sampling helpers with
opposite mask edges. A repeat-sampler control must reproduce the seam; fixed tile
sampling must preserve opacity without changing object or diffuse/detail sampling.
"""

from pathlib import Path
import struct

import wgpu


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "crates/omsi-render/src"
shader_source = "\n".join(
    (SOURCE / name).read_text(encoding="utf-8")
    for name in ("shader.wgsl", "enhanced_common.wgsl", "enhanced.wgsl")
)
adapter = wgpu.gpu.request_adapter_sync(power_preference="low-power")
device = adapter.request_device_sync()
device.create_shader_module(code=shader_source)
print(f"Combined scene WGSL validated on {adapter.info['device']}")

# First/last texel columns and rows differ, as in the Praha 200 asphalt masks.
# Every chosen point touches an opaque edge or its clamped extension.
probes = """
@vertex
fn test_vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    return vec4<f32>(p[index], 0.0, 1.0);
}
@fragment
fn test_fragment() -> @location(0) vec4<f32> {
    let points = array<vec2<f32>, 10>(
        vec2<f32>(0.0, 0.0), vec2<f32>(0.0, 0.25), vec2<f32>(0.25, 0.0),
        vec2<f32>(1.0, 1.0), vec2<f32>(1.0, 0.75), vec2<f32>(0.75, 1.0),
        vec2<f32>(-0.125, 0.25), vec2<f32>(1.125, 0.75),
        vec2<f32>(0.25, 0.25), vec2<f32>(0.75, 0.75));
    let uv = points[u32(material.flags.x)];
    return vec4<f32>(sample_transmap(uv).a, sample_nightmap(uv).r,
        textureSample(t_diffuse, s_diffuse, uv + vec2<f32>(1.0)).r,
        textureSample(t_light, s_diffuse, uv + vec2<f32>(1.0)).r);
}
"""
texture = device.create_texture(
    size=(2, 2, 1), format="rgba8unorm",
    usage=wgpu.TextureUsage.TEXTURE_BINDING | wgpu.TextureUsage.COPY_DST,
)
device.queue.write_texture(
    {"texture": texture},
    bytes([255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255]),
    {"bytes_per_row": 8, "rows_per_image": 2}, (2, 2, 1),
)
view = texture.create_view()


def render(source, terrain, anisotropy):
    module = device.create_shader_module(code=source + probes)
    pipeline = device.create_render_pipeline(
        layout="auto", vertex={"module": module, "entry_point": "test_vertex"},
        fragment={"module": module, "entry_point": "test_fragment",
                  "targets": [{"format": "rgba8unorm"}]},
    )
    params = [0.0] * 36  # MaterialParams: nine vec4s; extra.x is the tile flag.
    params[8] = float(terrain)
    uniform = device.create_buffer_with_data(
        data=struct.pack("<36f", *params),
        usage=wgpu.BufferUsage.UNIFORM | wgpu.BufferUsage.COPY_DST,
    )

    def sampler(mode):
        return device.create_sampler(
            address_mode_u=mode, address_mode_v=mode, address_mode_w=mode,
            mag_filter="linear", min_filter="linear", mipmap_filter="linear",
            max_anisotropy=anisotropy,
        )

    resources = {0: view, 1: sampler("repeat"), 2: {"buffer": uniform},
                 3: view, 4: view, 5: view, 11: sampler("clamp-to-edge")}
    # A control shader no longer uses s_tile, so auto-layout omits its binding.
    if "textureSample(t_trans, s_tile, uv)" not in source:
        del resources[11]
    group = device.create_bind_group(
        layout=pipeline.get_bind_group_layout(1),
        entries=[{"binding": k, "resource": v} for k, v in resources.items()],
    )
    empty = device.create_bind_group(layout=pipeline.get_bind_group_layout(0), entries=[])
    target = device.create_texture(
        size=(1, 1, 1), format="rgba8unorm",
        usage=wgpu.TextureUsage.RENDER_ATTACHMENT | wgpu.TextureUsage.COPY_SRC,
    )
    pixels = []
    for index in range(10):
        # Constant UV within each draw avoids derivatives across unrelated probes.
        params[32] = float(index)
        device.queue.write_buffer(uniform, 0, struct.pack("<36f", *params))
        encoder = device.create_command_encoder()
        render_pass = encoder.begin_render_pass(color_attachments=[{
            "view": target.create_view(), "resolve_target": None,
            "load_op": "clear", "store_op": "store", "clear_value": (0, 0, 0, 0),
        }])
        render_pass.set_pipeline(pipeline)
        render_pass.set_bind_group(0, empty)
        render_pass.set_bind_group(1, group)
        render_pass.draw(3)
        render_pass.end()
        device.queue.submit([encoder.finish()])
        pixels.append(tuple(device.queue.read_texture(
            {"texture": target}, {"bytes_per_row": 4, "rows_per_image": 1}, (1, 1, 1),
        )))
    return pixels


control_source = shader_source.replace(
    "textureSample(t_trans, s_tile, uv)", "textureSample(t_trans, s_diffuse, uv)"
).replace("textureSample(t_night, s_tile, uv)", "textureSample(t_night, s_diffuse, uv)")
assert control_source != shader_source, "Terrain sampling helpers were not found"
for anisotropy in (1, 16):
    fixed = render(shader_source, True, anisotropy)
    control = render(control_source, True, anisotropy)
    ordinary = render(shader_source, False, anisotropy)
    assert all(p[0] == 255 and p[1] == 255 for p in fixed), fixed
    assert control[1][0] < 200 and control[1][1] < 200, control
    assert ordinary == control, (ordinary, control)
    assert all(a[2:] == b[2:] for a, b in zip(fixed, control)), (fixed, control)
    assert fixed[1][2] < 200 and fixed[8][2] == 255, fixed
    print(f"PASS anisotropy {anisotropy}: mask/light-map edge {control[1][:2]} -> "
          f"{fixed[1][:2]}; object maps and repeating diffuse/detail unchanged")
