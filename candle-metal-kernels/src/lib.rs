use metal::{
    Buffer, CommandBufferRef, CompileOptions, ComputeCommandEncoderRef, ComputePipelineState,
    Device, Function, Library, MTLSize,
};
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::RwLock;

const AFFINE: &str = include_str!("affine.metal");
const INDEXING: &str = include_str!("indexing.metal");
const UNARY: &str = include_str!("unary.metal");
const BINARY: &str = include_str!("binary.metal");
const TERNARY: &str = include_str!("ternary.metal");
const CAST: &str = include_str!("cast.metal");
const REDUCE: &str = include_str!("reduce.metal");

fn linear_split(pipeline: &ComputePipelineState, length: usize) -> (MTLSize, MTLSize) {
    let size = length as u64;
    let width = std::cmp::min(pipeline.max_total_threads_per_threadgroup(), size);
    let count = (size + width - 1) / width;
    let thread_group_count = MTLSize {
        width: count,
        height: 1,
        depth: 1,
    };

    let thread_group_size = MTLSize {
        width,
        height: 1,
        depth: 1,
    };
    (thread_group_count, thread_group_size)
}

fn set_param<P: EncoderParam>(encoder: &ComputeCommandEncoderRef, position: u64, data: P) {
    <P as EncoderParam>::set_param(encoder, position, data)
}
trait EncoderParam {
    fn set_param(encoder: &ComputeCommandEncoderRef, position: u64, data: Self);
}
macro_rules! primitive {
    ($type:ty) => {
        impl EncoderParam for $type {
            fn set_param(encoder: &ComputeCommandEncoderRef, position: u64, data: Self) {
                encoder.set_bytes(
                    position,
                    core::mem::size_of::<$type>() as u64,
                    &data as *const $type as *const c_void,
                );
            }
        }
    };
}
primitive!(usize);
primitive!(u32);
primitive!(f32);

impl<T> EncoderParam for &[T] {
    fn set_param(encoder: &ComputeCommandEncoderRef, position: u64, data: Self) {
        encoder.set_bytes(
            position,
            core::mem::size_of_val(data) as u64,
            data.as_ptr() as *const c_void,
        );
    }
}

impl EncoderParam for &Buffer {
    fn set_param(encoder: &ComputeCommandEncoderRef, position: u64, data: Self) {
        encoder.set_buffer(position, Some(data), 0);
    }
}
impl EncoderParam for (&Buffer, usize) {
    fn set_param(encoder: &ComputeCommandEncoderRef, position: u64, data: Self) {
        encoder.set_buffer(position, Some(data.0), data.1 as u64);
    }
}
impl EncoderParam for &mut Buffer {
    fn set_param(encoder: &ComputeCommandEncoderRef, position: u64, data: Self) {
        encoder.set_buffer(position, Some(data), 0);
    }
}
impl EncoderParam for (&mut Buffer, usize) {
    fn set_param(encoder: &ComputeCommandEncoderRef, position: u64, data: Self) {
        encoder.set_buffer(position, Some(data.0), data.1 as u64);
    }
}

macro_rules! set_params {
    ($encoder:ident, ($($param:expr),+)) => (
        let mut _index = 0;
        $(
            set_param($encoder, _index, $param);
            _index += 1;
        )*
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Affine,
    Indexing,
    Unary,
    Binary,
    Ternary,
    Cast,
    Reduce,
}

macro_rules! ops{
    ($($name:ident),+) => {

        pub mod contiguous {
        pub struct Kernel(pub &'static str);
        $(
        pub mod $name {
            use super::Kernel;
            pub const FLOAT: Kernel = Kernel(concat!(stringify!($name), "_float"));
            pub const HALF: Kernel = Kernel(concat!(stringify!($name), "_half"));
            pub const BFLOAT: Kernel = Kernel(concat!(stringify!($name), "_bfloat"));
        }
        )+
            pub mod copy {
                use super::Kernel;
                pub const FLOAT: Kernel = Kernel("copy_float");
                pub const HALF: Kernel = Kernel("copy_half");
                pub const BFLOAT: Kernel = Kernel("copy_bfloat");
                pub const U32: Kernel = Kernel("copy_u32");
                pub const U8: Kernel = Kernel("copy_u8");
            }
        }

        pub mod strided {
        pub struct Kernel(pub &'static str);
        $(
        pub mod $name {
            use super::Kernel;
            pub const FLOAT: Kernel = Kernel(concat!(stringify!($name), "_float_strided"));
            pub const HALF: Kernel = Kernel(concat!(stringify!($name), "_half_strided"));
            pub const BFLOAT: Kernel = Kernel(concat!(stringify!($name), "_bfloat_strided"));
        }
        )+
            pub mod copy {
                use super::Kernel;
                pub const FLOAT: Kernel = Kernel("copy_float_strided");
                pub const HALF: Kernel = Kernel("copy_half_strided");
                pub const BFLOAT: Kernel = Kernel("copy_bfloat_strided");
                pub const U32: Kernel = Kernel("copy_u32_strided");
                pub const U8: Kernel = Kernel("copy_u8_strided");
            }
        }
    };
}

pub mod unary {
    ops!(cos, sin, exp, sqr, sqrt, neg, log, gelu, ceil, floor, round, erf, gelu_erf);
}
pub mod binary {
    ops!(add, sub, mul, div);
}

#[derive(thiserror::Error, Debug)]
pub enum MetalKernelError {
    #[error("Could not lock kernel map: {0}")]
    LockError(String),
    #[error("Error while loading library: {0}")]
    LoadLibraryError(String),
    #[error("Error while loading function: {0:?}")]
    LoadFunctionError(String),
    #[error("Failed to create compute function")]
    FailedToCreateComputeFunction,
    #[error("Failed to create pipeline")]
    FailedToCreatePipeline(String),
}

impl<T> From<std::sync::PoisonError<T>> for MetalKernelError {
    fn from(e: std::sync::PoisonError<T>) -> Self {
        Self::LockError(e.to_string())
    }
}

type KernelMap<T> = HashMap<&'static str, T>;
type Libraries = HashMap<Source, Library>;
type Pipelines = KernelMap<ComputePipelineState>;

#[derive(Debug, Default)]
pub struct Kernels {
    libraries: RwLock<Libraries>,
    pipelines: RwLock<Pipelines>,
}

impl Kernels {
    pub fn new() -> Self {
        let libraries = RwLock::new(Libraries::new());
        let pipelines = RwLock::new(Pipelines::new());
        Self {
            libraries,
            pipelines,
        }
    }

    fn get_library_source(&self, source: Source) -> &'static str {
        match source {
            Source::Affine => AFFINE,
            Source::Unary => UNARY,
            Source::Binary => BINARY,
            Source::Ternary => TERNARY,
            Source::Indexing => INDEXING,
            Source::Cast => CAST,
            Source::Reduce => REDUCE,
        }
    }

    pub fn load_library(
        &self,
        device: &Device,
        source: Source,
    ) -> Result<Library, MetalKernelError> {
        let mut libraries = self.libraries.write()?;
        if let Some(lib) = libraries.get(&source) {
            Ok(lib.clone())
        } else {
            let source_content = self.get_library_source(source);
            let lib = device
                .new_library_with_source(source_content, &CompileOptions::new())
                .map_err(|e| MetalKernelError::LoadLibraryError(e.to_string()))?;
            libraries.insert(source, lib.clone());
            Ok(lib)
        }
    }

    fn load_function(
        &self,
        device: &Device,
        source: Source,
        name: &'static str,
    ) -> Result<Function, MetalKernelError> {
        let func = self
            .load_library(device, source)?
            .get_function(name, None)
            .map_err(|e| MetalKernelError::LoadFunctionError(e.to_string()))?;
        Ok(func)
        // let mut funcs = self.funcs.write()?;
        // if let Some(func) = funcs.get(name) {
        //     Ok(func.clone())
        // } else {
        //     funcs.insert(name, func.clone());
        //     Ok(func)
        // }
    }

    pub fn load_pipeline(
        &self,
        device: &Device,
        source: Source,
        name: &'static str,
    ) -> Result<ComputePipelineState, MetalKernelError> {
        let mut pipelines = self.pipelines.write()?;
        if let Some(pipeline) = pipelines.get(name) {
            Ok(pipeline.clone())
        } else {
            let func = self.load_function(device, source, name)?;
            let pipeline = device
                .new_compute_pipeline_state_with_function(&func)
                .map_err(|e| MetalKernelError::FailedToCreatePipeline(e.to_string()))?;
            pipelines.insert(name, pipeline.clone());

            Ok(pipeline)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn call_unary_contiguous(
    device: &Device,
    command_buffer: &CommandBufferRef,
    kernels: &Kernels,
    kernel_name: unary::contiguous::Kernel,
    length: usize,
    input: &Buffer,
    output: &Buffer,
) -> Result<(), MetalKernelError> {
    let pipeline = kernels.load_pipeline(device, Source::Unary, kernel_name.0)?;
    let encoder = command_buffer.new_compute_command_encoder();
    encoder.set_compute_pipeline_state(&pipeline);

    set_params!(encoder, (length, input, output));

    let (thread_group_count, thread_group_size) = linear_split(&pipeline, length);
    encoder.dispatch_thread_groups(thread_group_count, thread_group_size);
    encoder.end_encoding();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn call_unary_strided(
    device: &Device,
    command_buffer: &CommandBufferRef,
    kernels: &Kernels,
    name: unary::strided::Kernel,
    shape: &[usize],
    input: &Buffer,
    strides: &[usize],
    offset: usize,
    output: &Buffer,
    output_offset: usize,
) -> Result<(), MetalKernelError> {
    let pipeline = kernels.load_pipeline(device, Source::Unary, name.0)?;

    let num_dims: usize = shape.len();
    let encoder = command_buffer.new_compute_command_encoder();
    encoder.set_compute_pipeline_state(&pipeline);

    let length: usize = shape.iter().product();
    set_params!(
        encoder,
        (
            length,
            num_dims,
            shape,
            strides,
            (input, offset),
            (output, output_offset)
        )
    );

    let width: usize = shape.iter().product();
    let (thread_group_count, thread_group_size) = linear_split(&pipeline, width);

    encoder.dispatch_thread_groups(thread_group_count, thread_group_size);
    encoder.end_encoding();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn call_binary_contiguous(
    device: &Device,
    command_buffer: &CommandBufferRef,
    kernels: &Kernels,
    kernel_name: binary::contiguous::Kernel,
    length: usize,
    left: &Buffer,
    right: &Buffer,
    output: &Buffer,
) -> Result<(), MetalKernelError> {
    let pipeline = kernels.load_pipeline(device, Source::Binary, kernel_name.0)?;

    let encoder = command_buffer.new_compute_command_encoder();
    encoder.set_compute_pipeline_state(&pipeline);

    set_params!(encoder, (length, left, right, output));

    let (thread_group_count, thread_group_size) = linear_split(&pipeline, length);

    encoder.dispatch_thread_groups(thread_group_count, thread_group_size);
    encoder.end_encoding();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn call_binary_strided(
    device: &Device,
    command_buffer: &CommandBufferRef,
    kernels: &Kernels,
    name: binary::strided::Kernel,
    shape: &[usize],
    left_input: &Buffer,
    left_strides: &[usize],
    left_offset: usize,
    right_input: &Buffer,
    right_strides: &[usize],
    right_offset: usize,
    output: &Buffer,
) -> Result<(), MetalKernelError> {
    let pipeline = kernels.load_pipeline(device, Source::Binary, name.0)?;

    let num_dims: usize = shape.len();
    let encoder = command_buffer.new_compute_command_encoder();
    let width: usize = shape.iter().product();
    encoder.set_compute_pipeline_state(&pipeline);

    let length: usize = shape.iter().product();

    set_params!(
        encoder,
        (
            length,
            num_dims,
            shape,
            left_strides,
            right_strides,
            (left_input, left_offset),
            (right_input, right_offset),
            output
        )
    );

    let (thread_group_count, thread_group_size) = linear_split(&pipeline, width);

    encoder.dispatch_thread_groups(thread_group_count, thread_group_size);
    encoder.end_encoding();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn call_cast_contiguous(
    device: &Device,
    command_buffer: &CommandBufferRef,
    kernels: &Kernels,
    kernel_name: &'static str,
    length: usize,
    input: &Buffer,
    input_offset: usize,
    output: &Buffer,
) -> Result<(), MetalKernelError> {
    let pipeline = kernels.load_pipeline(device, Source::Cast, kernel_name)?;

    let encoder = command_buffer.new_compute_command_encoder();
    encoder.set_compute_pipeline_state(&pipeline);

    set_params!(encoder, (length, (input, input_offset), output));

    let (thread_group_count, thread_group_size) = linear_split(&pipeline, length);
    encoder.dispatch_thread_groups(thread_group_count, thread_group_size);
    encoder.end_encoding();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn call_cast_strided(
    device: &Device,
    command_buffer: &CommandBufferRef,
    kernels: &Kernels,
    kernel_name: &'static str,
    shape: &[usize],
    input: &Buffer,
    input_strides: &[usize],
    input_offset: usize,
    output: &Buffer,
) -> Result<(), MetalKernelError> {
    let pipeline = kernels.load_pipeline(device, Source::Cast, kernel_name)?;

    let encoder = command_buffer.new_compute_command_encoder();
    encoder.set_compute_pipeline_state(&pipeline);

    let length: usize = shape.iter().product();

    set_params!(
        encoder,
        (
            length,
            shape.len(),
            shape,
            input_strides,
            (input, input_offset),
            output
        )
    );

    let (thread_group_count, thread_group_size) = linear_split(&pipeline, length);

    encoder.dispatch_thread_groups(thread_group_count, thread_group_size);
    encoder.end_encoding();
    Ok(())
}

pub fn call_reduce_contiguous(
    device: &Device,
    command_buffer: &CommandBufferRef,
    kernels: &Kernels,
    kernel_name: &'static str,
    length: usize,
    out_length: usize,
    input: &Buffer,
    input_offset: usize,
    output: &Buffer,
) -> Result<(), MetalKernelError> {
    let pipeline = kernels.load_pipeline(device, Source::Reduce, kernel_name)?;
    let elements_to_sum = length / out_length;

    let encoder = command_buffer.new_compute_command_encoder();
    encoder.set_compute_pipeline_state(&pipeline);

    set_params!(
        encoder,
        (length, elements_to_sum, (input, input_offset), output)
    );

    let thread_group_count = MTLSize {
        width: out_length as u64,
        height: 1,
        depth: 1,
    };

    let width = std::cmp::min(
        pipeline.max_total_threads_per_threadgroup(),
        (elements_to_sum as u64 + 2 - 1) / 2,
    )
    .next_power_of_two();

    let thread_group_size = MTLSize {
        width,
        height: 1,
        depth: 1,
    };

    encoder.dispatch_thread_groups(thread_group_count, thread_group_size);
    encoder.end_encoding();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn call_last_softmax(
    device: &Device,
    command_buffer: &CommandBufferRef,
    kernels: &Kernels,
    kernel_name: &'static str,
    length: usize,
    elements_to_sum: usize,
    input: &Buffer,
    output: &Buffer,
) -> Result<(), MetalKernelError> {
    let pipeline = kernels.load_pipeline(device, Source::Reduce, kernel_name)?;
    let encoder = command_buffer.new_compute_command_encoder();
    encoder.set_compute_pipeline_state(&pipeline);

    set_params!(encoder, (length, elements_to_sum, input, output));

    let out_length = length / elements_to_sum;

    let thread_group_count = MTLSize {
        width: out_length as u64,
        height: 1,
        depth: 1,
    };

    let width = std::cmp::min(
        pipeline.max_total_threads_per_threadgroup(),
        elements_to_sum as u64,
    )
    .next_power_of_two();

    let thread_group_size = MTLSize {
        width,
        height: 1,
        depth: 1,
    };

    encoder.dispatch_thread_groups(thread_group_count, thread_group_size);
    encoder.end_encoding();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn call_affine(
    device: &Device,
    command_buffer: &CommandBufferRef,
    kernels: &Kernels,
    name: &'static str,
    size: usize,
    input: &Buffer,
    output: &Buffer,
    mul: f32,
    add: f32,
) -> Result<(), MetalKernelError> {
    let pipeline = kernels.load_pipeline(device, Source::Affine, name)?;

    let encoder = command_buffer.new_compute_command_encoder();
    encoder.set_compute_pipeline_state(&pipeline);

    set_params!(encoder, (size, mul, add, input, output));

    let (thread_group_count, thread_group_size) = linear_split(&pipeline, size);
    encoder.dispatch_thread_groups(thread_group_count, thread_group_size);
    encoder.end_encoding();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn call_affine_strided(
    device: &Device,
    command_buffer: &CommandBufferRef,
    kernels: &Kernels,
    name: &'static str,
    shape: &[usize],
    input: &Buffer,
    input_stride: &[usize],
    input_offset: usize,
    output: &Buffer,
    mul: f32,
    add: f32,
) -> Result<(), MetalKernelError> {
    let pipeline = kernels.load_pipeline(device, Source::Affine, name)?;
    let size: usize = shape.iter().product();

    let encoder = command_buffer.new_compute_command_encoder();
    encoder.set_compute_pipeline_state(&pipeline);

    set_params!(
        encoder,
        (
            size,
            shape.len(),
            shape,
            input_stride,
            mul,
            add,
            (input, input_offset),
            output
        )
    );

    let (thread_group_count, thread_group_size) = linear_split(&pipeline, size);
    encoder.dispatch_thread_groups(thread_group_count, thread_group_size);
    encoder.end_encoding();
    Ok(())
}

pub fn call_where_cond_strided(
    device: &Device,
    command_buffer: &CommandBufferRef,
    kernels: &Kernels,
    name: &'static str,
    shape: &[usize],
    cond: &Buffer,
    (cond_stride, cond_offset): (&[usize], usize),
    left: &Buffer,
    (left_stride, left_offset): (&[usize], usize),
    right: &Buffer,
    (right_stride, right_offset): (&[usize], usize),
    output: &Buffer,
) -> Result<(), MetalKernelError> {
    let pipeline = kernels.load_pipeline(device, Source::Ternary, name)?;

    let encoder = command_buffer.new_compute_command_encoder();
    encoder.set_compute_pipeline_state(&pipeline);

    let size: usize = shape.iter().product();
    let rank = shape.len();

    set_params!(
        encoder,
        (
            size,
            rank,
            shape,
            cond_stride,
            left_stride,
            right_stride,
            (cond, cond_offset),
            (left, left_offset),
            (right, right_offset),
            output
        )
    );

    let (thread_group_count, thread_group_size) = linear_split(&pipeline, size);

    encoder.dispatch_thread_groups(thread_group_count, thread_group_size);
    encoder.end_encoding();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn call_index_select(
    device: &Device,
    command_buffer: &CommandBufferRef,
    kernels: &Kernels,
    name: &'static str,
    shape: &[usize],
    ids_size: usize,
    dim: usize,
    input: &Buffer,
    ids: &Buffer,
    output: &Buffer,
) -> Result<(), MetalKernelError> {
    let left_size: usize = shape[..dim].iter().product();
    let right_size: usize = shape[dim + 1..].iter().product();
    let src_dim_size = shape[dim];
    let dst_el = ids_size * left_size * right_size;

    let pipeline = kernels.load_pipeline(device, Source::Indexing, name)?;

    let encoder = command_buffer.new_compute_command_encoder();

    encoder.set_compute_pipeline_state(&pipeline);

    set_params!(
        encoder,
        (
            dst_el,
            left_size,
            src_dim_size,
            right_size,
            ids_size,
            input,
            ids,
            output
        )
    );

    let (thread_group_count, thread_group_size) = linear_split(&pipeline, dst_el);

    encoder.dispatch_thread_groups(thread_group_count, thread_group_size);
    encoder.end_encoding();
    Ok(())
}

#[cfg(test)]
mod tests;