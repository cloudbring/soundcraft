//! Integration tests, built as one test binary: each file is a module.

mod auto_pan;
mod fuzz;
mod loudness_oracle;
mod offline_meters;
mod offline_normalize;
mod plugins;
mod speed_ramp;
mod transients;

#[path = "../../../audio-io/tests/support/oracle.rs"]
mod oracle;
