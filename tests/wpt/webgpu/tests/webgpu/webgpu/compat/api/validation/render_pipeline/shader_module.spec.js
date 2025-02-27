/**
* AUTO-GENERATED - DO NOT EDIT. Source: https://github.com/gpuweb/cts
**/export const description = `
Tests limitations of createRenderPipeline related to shader modules in compat mode.
`;import { makeTestGroup } from '../../../../../common/framework/test_group.js';
import { CompatibilityTest } from '../../../compatibility_test.js';

export const g = makeTestGroup(CompatibilityTest);

g.test('sample_mask').
desc(
  `
Tests that you can not create a render pipeline with a shader module that uses sample_mask in compat mode.

- Test that a pipeline with a shader that uses sample_mask fails.
- Test that a pipeline that references a module that has a shader that uses sample_mask
  but the pipeline does not reference that shader succeeds.
    `
).
params((u) =>
u.combine('entryPoint', ['fsWithoutSampleMaskUsage', 'fsWithSampleMaskUsage'])
).
fn((t) => {
  const { entryPoint } = t.params;

  const module = t.device.createShaderModule({
    code: `
       @vertex fn vs() -> @builtin(position) vec4f {
            return vec4f(1);
        }
        struct Output {
            @builtin(sample_mask) mask_out: u32,
            @location(0) color : vec4f,
        }
        @fragment fn fsWithoutSampleMaskUsage() -> @location(0) vec4f {
            return vec4f(1.0, 1.0, 1.0, 1.0);
        }
        @fragment fn fsWithSampleMaskUsage() -> Output {
            var o: Output;
            // We need to make sure this sample_mask isn't optimized out even if its value equals "no op".
            o.mask_out = 0xFFFFFFFFu;
            o.color = vec4f(1.0, 1.0, 1.0, 1.0);
            return o;
        }
      `
  });

  const pipelineDescriptor = {
    layout: 'auto',
    vertex: {
      module,
      entryPoint: 'vs'
    },
    fragment: {
      module,
      entryPoint,
      targets: [
      {
        format: 'rgba8unorm'
      }]

    },
    multisample: {
      count: 4
    }
  };

  const isValid = entryPoint === 'fsWithoutSampleMaskUsage';
  t.expectGPUError(
    'validation',
    () => t.device.createRenderPipeline(pipelineDescriptor),
    !isValid
  );
});

g.test('interpolate').
desc(
  `
Tests that you can not create a render pipeline with a shader module that uses interpolate(linear) nor interpolate(...,sample) in compat mode.

- Test that a pipeline with a shader that uses interpolate(linear) or interpolate(sample) fails.
- Test that a pipeline that references a module that has a shader that uses interpolate(linear/sample)
  but the pipeline does not reference that shader succeeds.
    `
).
params((u) =>
u.
combine('interpolate', [
'',
'@interpolate(linear)',
'@interpolate(linear, sample)',
'@interpolate(perspective, sample)']
).
combine('entryPoint', [
'fsWithoutInterpolationUsage',
'fsWithInterpolationUsage1',
'fsWithInterpolationUsage2',
'fsWithInterpolationUsage3']
)
).
fn((t) => {
  const { entryPoint, interpolate } = t.params;

  const module = t.device.createShaderModule({
    code: `
        struct Vertex {
            @builtin(position) pos: vec4f,
            @location(0) ${interpolate} color : vec4f,
        };
        @vertex fn vs() -> Vertex {
            var v: Vertex;
            v.pos = vec4f(1);
            v.color = vec4f(1);
            return v;
        }
        @fragment fn fsWithoutInterpolationUsage() -> @location(0) vec4f {
            return vec4f(1);
        }
        @fragment fn fsWithInterpolationUsage1(v: Vertex) -> @location(0) vec4f {
            return vec4f(1);
        }
        @fragment fn fsWithInterpolationUsage2(v: Vertex) -> @location(0) vec4f {
            return v.pos;
        }
        @fragment fn fsWithInterpolationUsage3(v: Vertex) -> @location(0) vec4f {
            return v.color;
        }
      `
  });

  const pipelineDescriptor = {
    layout: 'auto',
    vertex: {
      module,
      entryPoint: 'vs'
    },
    fragment: {
      module,
      entryPoint,
      targets: [
      {
        format: 'rgba8unorm'
      }]

    }
  };

  const isValid = entryPoint === 'fsWithoutInterpolationUsage' || interpolate === '';
  t.expectGPUError(
    'validation',
    () => t.device.createRenderPipeline(pipelineDescriptor),
    !isValid
  );
});