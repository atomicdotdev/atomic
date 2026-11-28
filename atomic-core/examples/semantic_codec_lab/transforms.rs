//! Reversible numeric-column experiments; not production wire formats.
use anyhow::{bail, ensure, Context, Result};

pub const LIMIT: usize = 512 * 1024 * 1024;
pub const METHODS: [&str; 5] = ["raw", "rle", "byte_planes", "bit_pack", "parity"];

pub fn put(out: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        out.push((n as u8) | 128);
        n >>= 7;
    }
    out.push(n as u8);
}

fn get(input: &mut &[u8]) -> Result<u64> {
    let mut n = 0;
    for shift in (0..70).step_by(7) {
        let (&b, rest) = input.split_first().context("truncated varint")?;
        *input = rest;
        ensure!(shift != 63 || b <= 1, "varint overflow");
        n |= u64::from(b & 127) << shift;
        if b < 128 {
            ensure!(shift == 0 || b != 0, "noncanonical varint");
            return Ok(n);
        }
    }
    bail!("varint overflow")
}

fn count(mut bytes: &[u8]) -> Result<usize> {
    let mut n = 0;
    while !bytes.is_empty() {
        get(&mut bytes)?;
        n += 1;
    }
    Ok(n)
}

pub fn encode(method: u8, raw: &[u8]) -> Result<Vec<u8>> {
    ensure!(raw.len() <= LIMIT, "raw column limit");
    if method == 0 {
        return Ok(raw.to_vec());
    }
    ensure!((method as usize) < METHODS.len(), "unknown transform");
    let n = count(raw)?;
    let mut out = vec![];
    put(&mut out, n as u64);
    let mut input = raw;
    match method {
        1 => {
            let mut previous = None;
            let mut run = 0;
            while !input.is_empty() {
                let next = get(&mut input)?;
                if previous == Some(next) {
                    run += 1;
                } else {
                    if let Some(value) = previous {
                        put(&mut out, run);
                        put(&mut out, value);
                    }
                    previous = Some(next);
                    run = 1;
                }
            }
            if let Some(value) = previous {
                put(&mut out, run);
                put(&mut out, value);
            }
        }
        2 | 3 => {
            for start in (0..n).step_by(128) {
                let len = (n - start).min(128);
                let mut block = [0u64; 128];
                for value in &mut block[..len] {
                    *value = get(&mut input)?;
                }
                let values = &block[..len];
                if method == 2 {
                    let max = *values.iter().max().unwrap();
                    let width = (64 - max.leading_zeros()).div_ceil(8) as u8;
                    out.push(width);
                    for byte in 0..width {
                        for value in values {
                            out.push((value >> (8 * byte)) as u8);
                        }
                    }
                } else {
                    let base = *values.iter().min().unwrap();
                    put(&mut out, base);
                    let max = values.iter().map(|n| n - base).max().unwrap();
                    let width = (64 - max.leading_zeros()) as u8;
                    out.push(width);
                    let mut acc = 0u128;
                    let mut bits = 0;
                    for value in values {
                        acc |= u128::from(value - base) << bits;
                        bits += u32::from(width);
                        while bits >= 8 {
                            out.push(acc as u8);
                            acc >>= 8;
                            bits -= 8;
                        }
                    }
                    if bits > 0 {
                        out.push(acc as u8);
                    }
                }
            }
        }
        4 => {
            let mut even = vec![];
            let mut odd = vec![];
            for i in 0..n {
                let value = get(&mut input)?;
                put(if i % 2 == 0 { &mut even } else { &mut odd }, value);
            }
            put(&mut out, even.len() as u64);
            out.extend(even);
            out.extend(odd);
        }
        _ => unreachable!(),
    }
    Ok(out)
}

fn emit(out: &mut Vec<u8>, value: u64, limit: usize) -> Result<()> {
    let bytes = ((64 - value.leading_zeros()).max(1)).div_ceil(7) as usize;
    ensure!(
        out.len().checked_add(bytes).is_some_and(|n| n <= limit),
        "expanded column limit"
    );
    put(out, value);
    Ok(())
}

pub fn decode(method: u8, data: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    ensure!(
        raw_len <= LIMIT && data.len() <= LIMIT * 2 + 64,
        "column limit"
    );
    if method == 0 {
        ensure!(data.len() == raw_len, "raw length mismatch");
        return Ok(data.to_vec());
    }
    ensure!((method as usize) < METHODS.len(), "unknown transform");
    let mut input = data;
    let n = usize::try_from(get(&mut input)?)?;
    ensure!(n <= raw_len, "value count exceeds original byte length");
    ensure!(
        raw_len <= n.saturating_mul(10),
        "impossible numeric expanded length"
    );
    let mut out = vec![];
    out.try_reserve_exact(raw_len)?;
    match method {
        1 => {
            let mut remaining = n;
            while remaining > 0 {
                let run = usize::try_from(get(&mut input)?)?;
                ensure!(run > 0 && run <= remaining, "invalid run count");
                let value = get(&mut input)?;
                for _ in 0..run {
                    emit(&mut out, value, raw_len)?;
                }
                remaining -= run;
            }
        }
        2 | 3 => {
            for start in (0..n).step_by(128) {
                let len = (n - start).min(128);
                let base = if method == 3 { get(&mut input)? } else { 0 };
                let (&width, rest) = input.split_first().context("missing block width")?;
                input = rest;
                ensure!(
                    width <= if method == 2 { 8 } else { 64 },
                    "invalid block width"
                );
                let required = if method == 2 {
                    len * usize::from(width)
                } else {
                    (len * usize::from(width)).div_ceil(8)
                };
                ensure!(required <= input.len(), "truncated packed block");
                let (block, rest) = input.split_at(required);
                input = rest;
                let mut acc = 0u128;
                let mut bits = 0;
                let mut at = 0;
                for i in 0..len {
                    let value = if method == 2 {
                        let mut value = 0u64;
                        for byte in 0..width {
                            value |= u64::from(block[usize::from(byte) * len + i]) << (8 * byte);
                        }
                        value
                    } else {
                        while bits < u32::from(width) {
                            acc |= u128::from(block[at]) << bits;
                            at += 1;
                            bits += 8;
                        }
                        let mask = if width == 64 {
                            u64::MAX
                        } else {
                            (1u64 << width) - 1
                        };
                        let value = (acc as u64) & mask;
                        acc >>= width;
                        bits -= u32::from(width);
                        base.checked_add(value).context("packed integer overflow")?
                    };
                    emit(&mut out, value, raw_len)?;
                }
                if method == 3 {
                    ensure!(acc == 0, "nonzero packed padding");
                }
            }
        }
        4 => {
            let even_len = usize::try_from(get(&mut input)?)?;
            ensure!(even_len <= input.len(), "invalid parity split");
            let (mut even, mut odd) = input.split_at(even_len);
            for i in 0..n {
                emit(
                    &mut out,
                    get(if i % 2 == 0 { &mut even } else { &mut odd })?,
                    raw_len,
                )?;
            }
            ensure!(even.is_empty() && odd.is_empty(), "trailing parity values");
            input = &[];
        }
        _ => unreachable!(),
    }
    ensure!(
        input.is_empty() && out.len() == raw_len,
        "trailing bytes or expanded length mismatch"
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_numeric_extremes_runs_and_irregular_blocks() -> Result<()> {
        let mut seed = 7u64;
        for size in [0, 1, 2, 127, 128, 129, 4096] {
            let mut raw = vec![];
            for i in 0..size {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                put(
                    &mut raw,
                    match i % 7 {
                        0 => 0,
                        1 => u64::MAX,
                        2 => 1 << 63,
                        3 => 127,
                        4 => 128,
                        _ => seed,
                    },
                );
            }
            for method in 0..METHODS.len() as u8 {
                let encoded = encode(method, &raw)?;
                assert_eq!(decode(method, &encoded, raw.len())?, raw);
            }
            let mut repeated = vec![];
            for _ in 0..size {
                put(&mut repeated, 42);
            }
            for method in 0..METHODS.len() as u8 {
                assert_eq!(
                    decode(method, &encode(method, &repeated)?, repeated.len())?,
                    repeated
                );
            }
        }
        Ok(())
    }
    #[test]
    fn rejects_bad_counts_lengths_widths_and_truncation() -> Result<()> {
        let mut raw = vec![];
        for n in 0..513 {
            put(&mut raw, n);
        }
        for method in 0..METHODS.len() as u8 {
            let encoded = encode(method, &raw)?;
            for end in 0..encoded.len() {
                assert!(decode(method, &encoded[..end], raw.len()).is_err());
            }
            let mut extra = encoded;
            extra.push(0);
            assert!(decode(method, &extra, raw.len()).is_err());
        }
        assert!(decode(1, &[1, 0, 0], 1).is_err());
        assert!(decode(2, &[1, 9], 1).is_err());
        assert!(decode(3, &[1, 0, 65], 1).is_err());
        assert!(decode(255, &[], 0).is_err());
        assert!(encode(1, &[0x80, 0]).is_err());
        assert!(decode(0, &[], LIMIT + 1).is_err());
        assert!(decode(1, &[0], LIMIT).is_err());
        Ok(())
    }
}
