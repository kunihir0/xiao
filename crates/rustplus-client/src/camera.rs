use crate::{
    error::{Error, Result},
    types::CameraRays,
};

const MAX_DIMENSION: u32 = 4096;
const MAX_PIXELS: u64 = 1_048_576;
const MAX_FRAME_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct FrameBuilder {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub received: u32,
    sample_pos: Vec<u16>,
}

impl FrameBuilder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(Error::InvalidCamera(
                "dimensions must be non-zero and bounded",
            ));
        }
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or(Error::InvalidCamera("dimension overflow"))?;
        if pixels > MAX_PIXELS {
            return Err(Error::InvalidCamera("frame is too large"));
        }
        let pixel_bytes = pixels
            .checked_mul(4)
            .ok_or(Error::InvalidCamera("pixel buffer overflow"))?
            as usize;
        let sample_bytes = pixels
            .checked_mul(2)
            .ok_or(Error::InvalidCamera("sample position overflow"))?;
        if pixel_bytes
            .checked_add(sample_bytes)
            .ok_or(Error::InvalidCamera("frame memory overflow"))?
            > MAX_FRAME_BYTES
        {
            return Err(Error::InvalidCamera("frame memory budget exceeded"));
        }
        let mut sample_pos = Vec::with_capacity(
            usize::try_from(sample_bytes)
                .map_err(|_| Error::InvalidCamera("sample position overflow"))?,
        );
        for y in 0..height {
            for x in 0..width {
                sample_pos.push(
                    u16::try_from(x)
                        .map_err(|_| Error::InvalidCamera("width exceeds sample format"))?,
                );
                sample_pos.push(
                    u16::try_from(y)
                        .map_err(|_| Error::InvalidCamera("height exceeds sample format"))?,
                );
            }
        }
        let mut generator = IndexGenerator::new(1337);
        for index in (1..pixels).rev() {
            let max = u32::try_from(index + 1)
                .map_err(|_| Error::InvalidCamera("sample index overflow"))?;
            let swap = usize::try_from(generator.next_int(max))
                .map_err(|_| Error::InvalidCamera("sample index overflow"))?;
            let current = usize::try_from(index)
                .map_err(|_| Error::InvalidCamera("sample index overflow"))?
                .checked_mul(2)
                .ok_or(Error::InvalidCamera("sample index overflow"))?;
            let target = swap
                .checked_mul(2)
                .ok_or(Error::InvalidCamera("sample index overflow"))?;
            sample_pos.swap(current, target);
            sample_pos.swap(current + 1, target + 1);
        }
        Ok(Self {
            width,
            height,
            pixels: vec![0; pixel_bytes],
            received: 0,
            sample_pos,
        })
    }

    pub fn ingest(&mut self, rays: &CameraRays) -> Result<usize> {
        if rays.sample_offset < 0 {
            return Err(Error::InvalidCamera("negative sample offset"));
        }
        let sample_count = self.sample_pos.len() / 2;
        let total_positions = self.sample_pos.len();
        let mut offset = usize::try_from(rays.sample_offset)
            .map_err(|_| Error::InvalidCamera("sample offset overflow"))?;
        if sample_count == 0 {
            return Err(Error::InvalidCamera("empty frame"));
        }
        offset = offset
            .checked_mul(2)
            .ok_or(Error::InvalidCamera("sample offset overflow"))?
            % total_positions;
        let mut pointer = 0usize;
        let mut lookback = [[0u8; 3]; 64];
        let mut decoded = 0usize;
        while pointer < rays.ray_data.len() {
            if usize::try_from(self.received)
                .ok()
                .and_then(|received| received.checked_add(decoded))
                .is_none_or(|count| count >= sample_count)
            {
                return Err(Error::InvalidCamera("ray payload exceeds frame samples"));
            }
            let token = rays.ray_data[pointer];
            pointer += 1;
            let sample = if token == 255 {
                let bytes = take(&rays.ray_data, &mut pointer, 3)?;
                let sample = [(bytes[0] << 2) | (bytes[1] >> 6), bytes[1] & 63, bytes[2]];
                let slot = ((3 * (u16::from(sample[0]) / 128)
                    + 5 * (u16::from(sample[1]) / 16)
                    + 7 * u16::from(sample[2]))
                    & 63) as usize;
                lookback[slot] = sample;
                sample
            } else {
                match token & 192 {
                    0 => lookback[(token & 63) as usize],
                    64 => {
                        let old = lookback[(token & 63) as usize];
                        let byte = take(&rays.ray_data, &mut pointer, 1)?[0];
                        [
                            old[0].wrapping_add(byte >> 3).wrapping_sub(15),
                            old[1].wrapping_add(byte & 7).wrapping_sub(3),
                            old[2],
                        ]
                    }
                    128 => {
                        let old = lookback[(token & 63) as usize];
                        let byte = take(&rays.ray_data, &mut pointer, 1)?[0];
                        [old[0].wrapping_add(byte).wrapping_sub(127), old[1], old[2]]
                    }
                    _ => {
                        let bytes = take(&rays.ray_data, &mut pointer, 2)?;
                        let sample = [(bytes[0] << 2) | (bytes[1] >> 6), bytes[1] & 63, token & 63];
                        let slot = ((3 * (u16::from(sample[0]) / 128)
                            + 5 * (u16::from(sample[1]) / 16)
                            + 7 * u16::from(sample[2]))
                            & 63) as usize;
                        lookback[slot] = sample;
                        sample
                    }
                }
            };
            let t = sample[0];
            let r = sample[1];
            let material = sample[2];
            let first = offset;
            let second = first
                .checked_add(1)
                .ok_or(Error::InvalidCamera("sample offset overflow"))?
                % total_positions;
            offset = second
                .checked_add(1)
                .ok_or(Error::InvalidCamera("sample offset overflow"))?
                % total_positions;
            let x = usize::from(self.sample_pos[first]);
            let y = usize::from(
                (self.height as u16)
                    .checked_sub(self.sample_pos[second])
                    .and_then(|value| value.checked_sub(1))
                    .ok_or(Error::InvalidCamera("sample position out of bounds"))?,
            );
            let pixel = y
                .checked_mul(self.width as usize)
                .and_then(|v| v.checked_add(x))
                .and_then(|v| v.checked_mul(4))
                .ok_or(Error::InvalidCamera("pixel index overflow"))?;
            let distance = f64::from(t) / 1023.0;
            let alignment = f64::from(r) / 63.0;
            let color = match material {
                0 => [0.5, 0.5, 0.5],
                1 => [0.8, 0.7, 0.7],
                2 => [0.3, 0.7, 1.0],
                3 => [0.6, 0.6, 0.6],
                4 => [0.7, 0.7, 0.7],
                5 => [0.8, 0.6, 0.4],
                6 => [1.0, 0.4, 0.4],
                7 => [1.0, 0.1, 0.1],
                _ => [1.0, 1.0, 1.0],
            };
            let lighting = 0.3 + 0.7 * alignment;
            let fog = distance.powf(1.5).clamp(0.0, 1.0);
            let sky = [
                208.0 - (f64::from(y as u32) / f64::from(self.height)) * 40.0,
                230.0 - (f64::from(y as u32) / f64::from(self.height)) * 20.0,
                252.0,
            ];
            for channel in 0..3 {
                let shaded = lighting * color[channel] * 255.0;
                self.pixels[pixel + channel] =
                    (shaded * (1.0 - fog) + sky[channel] * fog).clamp(0.0, 255.0) as u8;
            }
            self.pixels[pixel + 3] = 255;
            self.received = self.received.saturating_add(1);
            decoded += 1;
        }
        Ok(decoded)
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        u64::from(self.received) >= u64::from(self.width) * u64::from(self.height)
    }
    #[must_use]
    pub fn image(&self) -> CameraImage {
        CameraImage {
            width: self.width,
            height: self.height,
            rgba: self.pixels.clone(),
        }
    }
}

fn take<'a>(data: &'a [u8], pointer: &mut usize, count: usize) -> Result<&'a [u8]> {
    let end = pointer
        .checked_add(count)
        .ok_or(Error::InvalidCamera("ray data length overflow"))?;
    let bytes = data
        .get(*pointer..end)
        .ok_or(Error::InvalidCamera("truncated ray token"))?;
    *pointer = end;
    Ok(bytes)
}

#[derive(Debug, Clone, Copy)]
struct IndexGenerator {
    state: u32,
}
impl IndexGenerator {
    fn new(seed: u32) -> Self {
        let mut this = Self { state: seed };
        this.next_state();
        this
    }
    fn next_state(&mut self) -> u32 {
        let t = self.state as i32;
        let mut value = t;
        value ^= value.wrapping_shl(13);
        value ^= (value as u32 >> 17) as i32;
        value ^= value.wrapping_shl(5);
        self.state = value as u32;
        if t >= 0 {
            t as u32
        } else {
            (4_294_967_295_i64 + i64::from(t) - 1) as u32
        }
    }
    fn next_int(&mut self, max: u32) -> u32 {
        if max == 0 {
            0
        } else {
            let mut value =
                ((u64::from(self.next_state()) * u64::from(max)) / 4_294_967_295) as i32;
            if value < 0 {
                value = (max as i32).wrapping_add(value).wrapping_sub(1);
            }
            value as u32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_sample_position_fixture_places_first_ray() {
        let fixture = include_str!("../tests/fixtures/legacy-wire.txt");
        let encoded = fixture
            .lines()
            .find_map(|line| line.strip_prefix("camera_2x2_single_ray = "))
            .expect("camera fixture");
        let ray_data = encoded
            .split_whitespace()
            .map(|byte| u8::from_str_radix(byte, 16).expect("hex fixture"))
            .collect::<Vec<_>>();
        let mut frame = FrameBuilder::new(2, 2).expect("valid dimensions");
        let decoded = frame
            .ingest(&CameraRays {
                vertical_fov: 1.0,
                sample_offset: 0,
                ray_data,
                distance: 1.0,
            })
            .expect("valid ray fixture");
        assert_eq!(decoded, 1);
        assert_eq!(&frame.image().rgba[4..8], &[38, 38, 38, 255]);
    }
}
