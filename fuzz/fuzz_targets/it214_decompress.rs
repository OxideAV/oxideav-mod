#![no_main]

//! Raw fuzzing of the Impulse Tracker 2.14 sample decompressor.
//!
//! `it_decode` only reaches `it::decompress_it214` when a mutated file
//! keeps a valid `IMPS` header whose pointer lands on plausible block
//! data, so the bit-width state machine gets little of that budget.
//! Here the fuzz bytes *are* the compressed body: the first byte picks
//! the sample width / the double-delta variant and scales the frame
//! count, the rest is the block stream (16-bit block lengths that
//! overrun the data, width-change codes selecting invalid widths, bit
//! streams that end mid-value). The contract is "returns, bounded":
//! never more frames than asked for or than the data could encode.

use libfuzzer_sys::fuzz_target;
use oxideav_mod::it::decompress_it214;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, body)) = data.split_first() else {
        return;
    };
    let sixteen = mode & 1 != 0;
    let double_delta = mode & 2 != 0;
    let frames = ((mode >> 2) as usize + 1) * 1200;
    let out = decompress_it214(body, frames, sixteen, double_delta);
    assert!(out.frames.len() <= frames);
    assert!(out.frames.len() <= body.len() * 8);
    assert!(out.consumed <= body.len());
    assert!(out.exact_blocks + out.short_blocks <= out.blocks);
});
