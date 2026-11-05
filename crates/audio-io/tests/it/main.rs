//! Integration tests, built as one test binary: each file is a module.

mod decode_oracle;
mod decode_read_list;
mod file_format;
mod float_headroom;
mod m4a_edit_list;
mod malformed;
mod mp4_length;
mod roundtrip;
mod stream;
mod wav_empty_data;

#[path = "../support/oracle.rs"]
mod oracle;
