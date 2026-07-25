use std::env;
use std::fs;
use std::path::PathBuf;

const DEFAULT_VECTOR_CAPACITY: usize = 2048;

fn main() {
    println!("cargo:rerun-if-env-changed=KOKO_VECTOR_CAPACITY");

    let capacity = env::var("KOKO_VECTOR_CAPACITY")
        .map(|value| {
            value.parse::<usize>().unwrap_or_else(|_| {
                panic!(
                    "KOKO_VECTOR_CAPACITY must be a positive power-of-two integer, got {value:?}"
                )
            })
        })
        .unwrap_or(DEFAULT_VECTOR_CAPACITY);
    assert!(
        capacity >= 64 && capacity.is_power_of_two(),
        "KOKO_VECTOR_CAPACITY must be a power of two of at least 64, got {capacity}"
    );

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo always sets OUT_DIR"))
        .join("vector_capacity.rs");
    fs::write(
        output,
        format!(
            "/// Maximum rows in one vectorized execution batch.\n\
             ///\n\
             /// Defaults to {DEFAULT_VECTOR_CAPACITY}; set `KOKO_VECTOR_CAPACITY` while building\n\
             /// to tune this compile-time invariant for a target workload.\n\
             pub const VECTOR_CAPACITY: usize = {capacity};\n"
        ),
    )
    .expect("write generated vector capacity");
}
