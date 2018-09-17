use fr32::Fr32Ary;
use std::cmp;
use std::fmt::Debug;
use std::io::{self, Read, Result, Write};
use std::iter::FromIterator;

use bitvec::{self, BitVec};
use itertools::Itertools;

pub const FR_UNPADDED_BITS: usize = 254;
pub const FR_PADDED_BITS: usize = 256;

pub fn write_padded<W: ?Sized>(source: &[u8], target: &mut W) -> io::Result<u64>
where
    W: Write,
{
    let mut written: u64 = 0;
    for chunk in BitVec::<bitvec::LittleEndian, u8>::from(source)
        .into_iter()
        .chunks(FR_UNPADDED_BITS)
        .into_iter()
    {
        let mut bits = BitVec::<bitvec::LittleEndian, u8>::from_iter(chunk);

        // pad
        while bits.len() < FR_PADDED_BITS {
            bits.push(false);
        }

        let out = &bits.into_boxed_slice();

        target.write_all(&out)?;
        written += out.len() as u64;
    }
    Ok(written)
}

// offset and num_bytes are based on the unpadded data, so
// if [0, 1, ..., 255] was the original unpadded data, offset 3 and len 4 would return
// [2, 3, 4, 5].
pub fn write_unpadded<W: ?Sized>(
    source: &[u8],
    target: &mut W,
    offset: usize,
    len: usize,
) -> io::Result<u64>
where
    W: Write,
{
    // assuming 32byte aligned (4 u64 = 1Fr)

    let offset_bits = offset * 8;
    let len_bits = len * 8;
    let fr_count_offset = {
        let tmp = (offset_bits as f32 / FR_UNPADDED_BITS as f32).ceil() as usize;
        if tmp > 0 {
            tmp - 1
        } else {
            tmp
        }
    };
    let fr_count_len = (len_bits as f32 / FR_UNPADDED_BITS as f32).ceil() as usize + 1;

    let start_padded = (fr_count_offset * FR_PADDED_BITS as usize) / 8;
    let end_padded = ::std::cmp::min(
        source.len(),
        start_padded + (fr_count_len * FR_PADDED_BITS as usize) / 8,
    );

    println!(
        "(0..{}..{}..{}) {} {} {} {}",
        offset,
        offset + len,
        source.len(),
        fr_count_offset,
        fr_count_len,
        start_padded,
        end_padded
    );
    let padded_chunks = BitVec::<bitvec::LittleEndian, u8>::from(&source[start_padded..end_padded])
        .into_iter()
        .chunks(FR_PADDED_BITS);

    let unpadded_chunks = padded_chunks
        .into_iter()
        .flat_map(|chunk| chunk.take(FR_UNPADDED_BITS))
        .chunks(8);
    let slices = unpadded_chunks
        .into_iter()
        .skip(offset - start_padded)
        .take(len)
        .map(|chunk| {
            let bits = BitVec::<bitvec::LittleEndian, u8>::from_iter(chunk);
            bits.into_boxed_slice()
        });

    let mut written = 0;
    for slice in slices {
        println!("slice: {:?}", &slice);
        target.write_all(&slice)?;
        written += slice.len() as u64;
    }
    assert_eq!(written as usize, len);
    Ok(written)
}

pub struct Fr32Writer<W> {
    inner: W,
    prefix: u8,
    prefix_size: usize,
    bits_needed: usize,
}

pub struct Fr32Reader<R> {
    _inner: R,
}

impl<W: Write> Write for Fr32Writer<W> {
    fn write(&mut self, mut buf: &[u8]) -> Result<usize> {
        let bytes_remaining = buf.len();
        let mut source_bytes_written = 0;

        while source_bytes_written < bytes_remaining {
            let (remainder, remainder_size, bytes_consumed, bytes_to_write, more) =
                self.process_bytes(&buf);

            source_bytes_written += bytes_consumed;

            if more {
                // We read a complete chunk and should continue.
                self.ensure_write(&bytes_to_write)?;
            //source_bytes_written += bytes_consumed;//bytes_to_write.len();
            } else {
                // We read an incomplete chunk, so this is the last iteration.
                // We must have consumed all the bytes in buf.
                assert!(buf.len() == bytes_consumed);
                assert!(bytes_consumed < 32);

                // Write those bytes but no more (not a whole 32-byte chunk).
                let real_length = buf.len();
                assert!(real_length <= bytes_to_write.len());

                let truncated = &bytes_to_write[0..real_length];
                self.ensure_write(truncated)?;
                //source_bytes_written += truncated.len();

                if self.prefix_size > 0 {
                    // Since this chunk was incomplete, what would have been the remainder was included as the last byte to write.
                    // We shouldn't write it now, though, because we may need to write more bytes later.
                    // However, we do need to save the prefix.
                    self.prefix = bytes_to_write[real_length];
                }

                break;
            }

            self.prefix = remainder;
            self.prefix_size = remainder_size;

            let residual_bytes_size = buf.len() - bytes_consumed;
            let residual_bytes = &buf[(buf.len() - residual_bytes_size)..buf.len()];
            buf = residual_bytes;
        }
        // TODO: proper accounting
        if source_bytes_written > buf.len() {
            Ok(bytes_remaining)
        } else {
            Ok(source_bytes_written)
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

impl<W: Write> Fr32Writer<W> {
    pub fn new(inner: W) -> Fr32Writer<W> {
        Fr32Writer {
            inner,
            prefix: 0,
            prefix_size: 0,
            bits_needed: FR_UNPADDED_BITS,
        }
    }
    // Tries to process bytes.
    // Returns result of (remainder, remainder size, bytes_consumed, byte output, complete). Remainder size is in bits.
    // Complete is true iff we read a complete chunk of data.
    pub fn process_bytes(&mut self, bytes: &[u8]) -> (u8, usize, usize, Fr32Ary, bool) {
        let bits_needed = self.bits_needed;
        let full_bytes_needed = bits_needed / 8;

        // The non-byte-aligned tail bits are the suffix and will become the final byte of output.
        let suffix_size = bits_needed % 8;

        // Anything left in the byte containing the suffix will become the remainder.
        let mut remainder_size = 8 - suffix_size;

        // Consume as many bytes as needed, unless there aren't enough.
        let bytes_to_consume = cmp::min(full_bytes_needed, bytes.len());
        let mut final_byte = 0;
        let mut bytes_consumed = bytes_to_consume;
        let mut incomplete = false;

        if bytes_to_consume <= bytes.len() {
            if remainder_size != 0 {
                if (bytes_to_consume + 1) > bytes.len() {
                    // Too few bytes were sent.
                    incomplete = true;
                } else {
                    // This iteration's remainder will be included in next iteration's output.
                    self.bits_needed = FR_UNPADDED_BITS - remainder_size;

                    // The last byte we consume is special.
                    final_byte = bytes[bytes_to_consume];

                    // Increment the count of consumed bytes, since we just consumed another.
                    bytes_consumed += 1;
                }
            }
        } else {
            // Too few bytes were sent.
            incomplete = true;
        }

        if incomplete {
            // Too few bytes were sent.

            // We will need the unsent bits next iteration.
            self.bits_needed = bits_needed - bytes.len();

            // We only consumed the bytes that were sent.
            bytes_consumed = bytes.len();

            // The current prefix and remainder have the same size; no padding is added in this iteration.
            remainder_size = self.prefix_size;
        }

        // Grab all the full bytes (excluding suffix) we intend to consume.
        let full_bytes = &bytes[0..bytes_to_consume];

        // The suffix is the last part of this iteration's output.
        // The remainder will be the first part of next iteration's output.
        let (suffix, remainder) = split_byte(final_byte, suffix_size);
        let out_bytes = assemble_bytes(self.prefix, self.prefix_size, full_bytes, suffix);
        (
            remainder,
            remainder_size,
            bytes_consumed,
            out_bytes,
            !incomplete,
        )
    }

    pub fn finish(&mut self) -> Result<usize> {
        if self.prefix_size > 0 {
            assert!(self.prefix_size <= 8);
            let b = self.prefix;
            self.ensure_write(&[b])?;
            self.flush()?;
            self.prefix_size = 0;
            self.prefix = 0;
            Ok(1)
        } else {
            Ok(0)
        }
    }

    fn ensure_write(&mut self, mut buffer: &[u8]) -> Result<usize> {
        let mut bytes_written = 0;

        while !buffer.is_empty() {
            let n = self.inner.write(buffer)?;

            buffer = &buffer[n..buffer.len()];
            bytes_written += n;
        }
        Ok(bytes_written)
    }
}

// Splits byte into two parts at position, pos.
// The more significant part is right-shifted by pos bits, and both parts are returned,
// least-significant first.
fn split_byte(byte: u8, pos: usize) -> (u8, u8) {
    if pos == 0 {
        return (0, byte);
    };
    let b = byte >> pos;
    let mask_size = 8 - pos;
    let mask = (0xff >> mask_size) << mask_size;
    let a = (byte & mask) >> mask_size;
    (a, b)
}

// Prepend prefix to bytes, shifting all bytes left by prefix_size.
fn assemble_bytes(mut prefix: u8, prefix_size: usize, bytes: &[u8], suffix: u8) -> Fr32Ary {
    assert!(bytes.len() <= 31);
    let mut out = [0u8; 32];
    out[0] = prefix;

    let left_shift = prefix_size;
    let right_shift = 8 - prefix_size;
    for (i, b) in bytes.iter().enumerate() {
        if prefix_size == 0 {
            out[i] |= b;
        } else {
            let shifted = b.wrapping_shl(left_shift as u32); // This may overflow 8 bits, truncating the most significant bits.
            out[i] = prefix | shifted;
            prefix = b >> right_shift;
        }
    }
    out[bytes.len()] = prefix | suffix.wrapping_shl(left_shift as u32);
    out
}

impl<R: Read> Fr32Reader<R> {
    pub fn new(inner: R) -> Fr32Reader<R> {
        Fr32Reader { _inner: inner }
    }
}

impl<R: Read + Debug> Read for Fr32Reader<R> {
    fn read(&mut self, _buf: &mut [u8]) -> Result<usize> {
        unimplemented!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, XorShiftRng};
    use std::io;

    fn write_test(bytes: &[u8], extra_bytes: &[u8]) -> (usize, Vec<u8>) {
        let mut data = Vec::new();

        let write_count = {
            let mut writer = Fr32Writer::new(&mut data);
            let mut count = writer.write(&bytes).unwrap();
            // This tests to make sure state is correctly maintained so we can restart writing mid-32-byte chunk.
            count += writer.write(extra_bytes).unwrap();
            writer.finish().unwrap();
            count
        };

        (write_count, data)
    }

    #[test]
    fn test_write() {
        let source = vec![
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 0xff, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
            16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 0xff, 9, 9,
        ];
        let extra = vec![9, 0xff];

        let (write_count, buf) = write_test(&source, &extra);
        assert_eq!(write_count, 68);
        assert_eq!(buf.len(), 69);

        for i in 0..31 {
            assert_eq!(buf[i], i as u8 + 1);
        }
        assert_eq!(buf[31], 63); // Six least significant bits of 0xff
        assert_eq!(buf[32], (1 << 2) | 0b11); // 7
        for i in 33..63 {
            assert_eq!(buf[i], (i as u8 - 31) << 2);
        }
        assert_eq!(buf[63], (0x0f << 2)); // 4-bits of ones, half of 0xff, shifted by two, followed by two bits of 0-padding.
        assert_eq!(buf[64], 0x0f | 9 << 4); // The last half of 0xff, 'followed' by 9.
        assert_eq!(buf[65], 9 << 4); // A shifted 9.
        assert_eq!(buf[66], 9 << 4); // Another.
        assert_eq!(buf[67], 0xf0); // The final 0xff is split into two bytes. Here is the first half.
        assert_eq!(buf[68], 0x0f); // And here is the second.
    }

    // Read is still unimplemented.
    // #[test]
    fn test_read() {
        let data = vec![2u8; 1000];

        let mut padded_data: Vec<u8> = vec![];
        {
            let mut pad_writer = Fr32Writer::new(&mut padded_data);
            pad_writer.write(&data).unwrap();
        }
        assert_ne!(data, &padded_data[..]);

        let mut unpadded_data: Vec<u8> = vec![];
        let mut unpad_reader = Fr32Reader::new(&padded_data[..]);

        let written = io::copy(&mut unpad_reader, &mut unpadded_data).unwrap();

        assert_eq!(data, unpadded_data);
    }

    #[test]
    fn test_write_padded() {
        let data = vec![255u8; 32];
        let mut padded = Vec::new();
        let written = write_padded(&data, &mut padded).unwrap();
        assert_eq!(written, 64);
        assert_eq!(padded.len(), 64);
        assert_eq!(&padded[0..31], &data[0..31]);
        assert_eq!(padded[31], 0b0011_1111);
        assert_eq!(padded[32], 0b0000_0011);
        assert_eq!(&padded[33..], vec![0u8; 31].as_slice());
    }

    #[test]
    fn test_write_padded_alt() {
        let mut source = vec![
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 0xff, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
            16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 0xff, 9, 9,
        ];
        // FIXME: This doesn't exercise the ability to write a second time, which is the point of the extra_bytes in write_test.
        source.extend(vec![9, 0xff]);

        let mut buf = Vec::new();
        write_padded(&source, &mut buf).unwrap();

        for i in 0..31 {
            assert_eq!(buf[i], i as u8 + 1);
        }
        assert_eq!(buf[31], 63); // Six least significant bits of 0xff
        assert_eq!(buf[32], (1 << 2) | 0b11); // 7
        for i in 33..63 {
            assert_eq!(buf[i], (i as u8 - 31) << 2);
        }
        assert_eq!(buf[63], (0x0f << 2)); // 4-bits of ones, half of 0xff, shifted by two, followed by two bits of 0-padding.
        assert_eq!(buf[64], 0x0f | 9 << 4); // The last half of 0xff, 'followed' by 9.
        assert_eq!(buf[65], 9 << 4); // A shifted 9.
        assert_eq!(buf[66], 9 << 4); // Another.
        assert_eq!(buf[67], 0xf0); // The final 0xff is split into two bytes. Here is the first half.
        assert_eq!(buf[68], 0x0f); // And here is the second.
    }

    #[test]
    fn test_read_write_padded() {
        let len = 1024;
        let data = vec![255u8; len];
        let mut padded = Vec::new();
        write_padded(&data, &mut padded).unwrap();

        let mut unpadded = Vec::new();
        write_unpadded(&padded, &mut unpadded, 0, len).unwrap();

        assert_eq!(data, unpadded);
    }

    #[test]
    fn test_read_write_padded_offset() {
        let rng = &mut XorShiftRng::from_seed([0x3dbe6259, 0x8d313d76, 0x3237db17, 0xe5bc0654]);

        let len = 1024;
        let data: Vec<u8> = (0..len).map(|_| rng.gen()).collect();
        let mut padded = Vec::new();
        write_padded(&data, &mut padded).unwrap();

        {
            let mut unpadded = Vec::new();
            write_unpadded(&padded, &mut unpadded, 0, 44).unwrap();
            let expected = &data[0..44];

            assert_eq!(expected.len(), unpadded.len());
            assert_eq!(expected, &unpadded[..]);
        }

        {
            let mut unpadded = Vec::new();
            write_unpadded(&padded, &mut unpadded, 44, 127).unwrap();
            let expected = &data[44..44 + 127];

            println!("data[0..44]: {:?}", &data[0..44]);
            println!("data[44..44+127]: {:?}", &data[44..44 + 127]);
            assert_eq!(expected.len(), unpadded.len());
            assert_eq!(expected, &unpadded[..]);
        }
    }
}
