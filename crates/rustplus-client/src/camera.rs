use crate::{error::{Error, Result}, types::CameraRays};

const MAX_DIMENSION: u32 = 2048;
const MAX_PIXELS: u64 = 4_194_304;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraImage { pub width: u32, pub height: u32, pub rgba: Vec<u8> }

#[derive(Debug, Clone)]
pub struct FrameBuilder {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub received: u32,
    sample_pos: Vec<(u16, u16)>,
}
impl FrameBuilder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION { return Err(Error::InvalidCamera("dimensions must be non-zero and bounded")); }
        let pixels = u64::from(width).checked_mul(u64::from(height)).ok_or(Error::InvalidCamera("dimension overflow"))?;
        if pixels > MAX_PIXELS { return Err(Error::InvalidCamera("frame is too large")); }
        let pixel_bytes = pixels.checked_mul(4).and_then(|n| usize::try_from(n).ok()).ok_or(Error::InvalidCamera("pixel buffer overflow"))?;
        let mut sample_pos = Vec::with_capacity(usize::try_from(pixels).map_err(|_| Error::InvalidCamera("sample buffer overflow"))?);
        for y in 0..height { for x in 0..width { sample_pos.push((u16::try_from(x).map_err(|_| Error::InvalidCamera("width exceeds sample format"))?, u16::try_from(y).map_err(|_| Error::InvalidCamera("height exceeds sample format"))?)); } }
        let mut generator = IndexGenerator::new(1337);
        for index in (1..pixels).rev() {
            let max = u32::try_from(index + 1).map_err(|_| Error::InvalidCamera("sample index overflow"))?;
            let swap = usize::try_from(generator.next_int(max)).map_err(|_| Error::InvalidCamera("sample index overflow"))?;
            sample_pos.swap(usize::try_from(index).map_err(|_| Error::InvalidCamera("sample index overflow"))?, swap);
        }
        Ok(Self { width, height, pixels: vec![0; pixel_bytes], received: 0, sample_pos })
    }

    pub fn ingest(&mut self, rays: &CameraRays) -> Result<usize> {
        if rays.sample_offset < 0 { return Err(Error::InvalidCamera("negative sample offset")); }
        let sample_count = self.sample_pos.len();
        if sample_count == 0 { return Err(Error::InvalidCamera("empty frame")); }
        if self.is_complete() { return if rays.ray_data.is_empty() { Ok(0) } else { Err(Error::InvalidCamera("excess samples after frame completion")) }; }
        let mut offset = usize::try_from(rays.sample_offset).map_err(|_| Error::InvalidCamera("sample offset overflow"))?;
        let slots = sample_count.checked_mul(2).ok_or(Error::InvalidCamera("sample slot overflow"))?;
        offset = offset.checked_mul(2).ok_or(Error::InvalidCamera("sample offset overflow"))? % slots;
        let mut pointer = 0usize;
        let mut lookback = [[0u8; 3]; 64];
        let mut decoded = 0usize;
        while pointer < rays.ray_data.len() {
            if decoded >= sample_count.saturating_sub(usize::try_from(self.received).unwrap_or(sample_count)) { return Err(Error::InvalidCamera("ray payload contains excess samples")); }
            let token = rays.ray_data[pointer]; pointer += 1;
            let sample = if token == 255 {
                let bytes = take(&rays.ray_data, &mut pointer, 3)?;
                let sample = [(bytes[0] << 2) | (bytes[1] >> 6), bytes[1] & 63, bytes[2]];
                let slot = ((3 * (u16::from(sample[0]) / 128) + 5 * (u16::from(sample[1]) / 16) + 7 * u16::from(sample[2])) & 63) as usize;
                lookback[slot] = sample; sample
            } else {
                match token & 192 {
                    0 => lookback[(token & 63) as usize],
                    64 => { let old = lookback[(token & 63) as usize]; let byte = take(&rays.ray_data, &mut pointer, 1)?[0]; [old[0].wrapping_add(byte >> 3).wrapping_sub(15), old[1].wrapping_add(byte & 7).wrapping_sub(3), old[2]] }
                    128 => { let old = lookback[(token & 63) as usize]; let byte = take(&rays.ray_data, &mut pointer, 1)?[0]; [old[0].wrapping_add(byte).wrapping_sub(127), old[1], old[2]] }
                    _ => { let bytes = take(&rays.ray_data, &mut pointer, 2)?; let sample = [(bytes[0] << 2) | (bytes[1] >> 6), bytes[1] & 63, token & 63]; let slot = ((3 * (u16::from(sample[0]) / 128) + 5 * (u16::from(sample[1]) / 16) + 7 * u16::from(sample[2])) & 63) as usize; lookback[slot] = sample; sample }
                }
            };
            let first = offset % slots;
            offset = (first + 2) % slots;
            let sample_pos = self.sample_pos[first / 2];
            let x = usize::from(sample_pos.0);
            let y = usize::from(self.height as u16 - 1 - sample_pos.1);
            let pixel = y.checked_mul(self.width as usize).and_then(|v| v.checked_add(x)).and_then(|v| v.checked_mul(4)).ok_or(Error::InvalidCamera("pixel index overflow"))?;
            let distance = f64::from(sample[0]) / 1023.0;
            let alignment = f64::from(sample[1]) / 63.0;
            let color = match sample[2] { 0 => [0.5,0.5,0.5], 1 => [0.8,0.7,0.7], 2 => [0.3,0.7,1.0], 3 => [0.6,0.6,0.6], 4 => [0.7,0.7,0.7], 5 => [0.8,0.6,0.4], 6 => [1.0,0.4,0.4], 7 => [1.0,0.1,0.1], _ => [1.0,1.0,1.0] };
            let lighting = 0.3 + 0.7 * alignment;
            let fog = distance.powf(1.5).clamp(0.0, 1.0);
            let sky = [208.0 - (y as f64 / f64::from(self.height)) * 40.0, 230.0 - (y as f64 / f64::from(self.height)) * 20.0, 252.0];
            for channel in 0..3 { self.pixels[pixel + channel] = (lighting * color[channel] * 255.0 * (1.0 - fog) + sky[channel] * fog).clamp(0.0, 255.0) as u8; }
            self.pixels[pixel + 3] = 255;
            decoded += 1;
        }
        self.received = self.received.checked_add(u32::try_from(decoded).map_err(|_| Error::InvalidCamera("sample count overflow"))?).ok_or(Error::InvalidCamera("sample count overflow"))?;
        Ok(decoded)
    }
    #[must_use] pub fn is_complete(&self) -> bool { u64::from(self.received) >= u64::from(self.width) * u64::from(self.height) }
    #[must_use] pub fn image(&self) -> CameraImage { CameraImage { width: self.width, height: self.height, rgba: self.pixels.clone() } }
}

fn take<'a>(data: &'a [u8], pointer: &mut usize, count: usize) -> Result<&'a [u8]> {
    let end = pointer.checked_add(count).ok_or(Error::InvalidCamera("ray data length overflow"))?;
    let bytes = data.get(*pointer..end).ok_or(Error::InvalidCamera("truncated ray token"))?;
    *pointer = end; Ok(bytes)
}

#[derive(Debug, Clone, Copy)]
struct IndexGenerator { state: u32 }
impl IndexGenerator {
    fn new(seed: u32) -> Self { let mut this = Self { state: seed }; this.next_state(); this }
    fn next_state(&mut self) -> u32 {
        let old = self.state as i32;
        let mut value = old;
        value ^= value << 13;
        value ^= (value as u32 >> 17) as i32;
        value ^= value << 5;
        self.state = value as u32;
        if old >= 0 { old as u32 } else { (4_294_967_295_i64 + i64::from(old) - 1) as u32 }
    }
    fn next_int(&mut self, max: u32) -> u32 {
        if max == 0 { return 0; }
        let mut value = ((u64::from(self.next_state()) * u64::from(max)) / 4_294_967_295) as i32;
        if value < 0 { value = max as i32 + value - 1; }
        value as u32
    }
}
