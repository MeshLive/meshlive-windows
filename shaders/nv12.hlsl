// 8-bit SDR NV12 on the UI's device, using single-texture plane views.
Texture2D<float> luma : register(t0);
Texture2D<float2> chroma : register(t1);
SamplerState linear_clamp : register(s0);
cbuffer Conversion : register(b0) {
    float4 red_row;
    float4 green_row;
    float4 blue_row;
    float4 geometry; // crop origin, inverse chroma width/height
    float4 location; // chroma sample origin
    float4 chroma_bounds; // valid crop bounds in normalized chroma coordinates
};
float4 vs_video(uint id : SV_VertexID) : SV_POSITION {
    float2 p = float2((id << 1) & 2, id & 2);
    return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);
}
float4 read_planes(float4 position) {
    float2 pixel = position.xy - 0.5 + geometry.xy;
    float y = luma.Load(int3(int2(pixel), 0));
    float2 uv = ((pixel - location.xy) * 0.5 + 0.5) * geometry.zw;
    uv = clamp(uv, chroma_bounds.xy, chroma_bounds.zw);
    float2 cbcr = chroma.SampleLevel(linear_clamp, uv, 0);
    return float4(y, cbcr, 1);
}
float4 ps_probe(float4 position : SV_POSITION) : SV_TARGET {
    return read_planes(position);
}
float4 ps_video(float4 position : SV_POSITION) : SV_TARGET {
    float4 value = read_planes(position);
    return float4(saturate(float3(dot(red_row, value), dot(green_row, value), dot(blue_row, value))), 1);
}
