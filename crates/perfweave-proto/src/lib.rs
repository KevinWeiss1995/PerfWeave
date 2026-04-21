#![allow(clippy::all)]
#![allow(missing_docs)]

pub mod v1 {
    tonic::include_proto!("perfweave.v1");
}

pub use v1::*;
