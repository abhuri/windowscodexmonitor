use std::{
    env,
    fs::File,
    io::{self, Write},
    path::Path,
};

const ICON_SIZES: &[u32] = &[16, 24, 32, 48, 64, 128, 256];

fn main() {
    println!("cargo:rerun-if-changed=assets/app-icon.svg");
    if env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let icon_path = Path::new(&env::var("OUT_DIR").expect("OUT_DIR is set")).join("app-icon.ico");
    write_icon(&icon_path).expect("could not create application icon");
    winres::WindowsResource::new()
        .set_icon(icon_path.to_str().expect("icon path is valid UTF-8"))
        .compile()
        .expect("could not embed application icon");
}

fn write_icon(path: &Path) -> io::Result<()> {
    let images: Vec<Vec<u8>> = ICON_SIZES.iter().copied().map(encode_bmp).collect();
    let header_size = 6 + ICON_SIZES.len() * 16;
    let mut file = File::create(path)?;
    write_u16(&mut file, 0)?;
    write_u16(&mut file, 1)?;
    write_u16(&mut file, ICON_SIZES.len() as u16)?;

    let mut offset = header_size as u32;
    for (size, image) in ICON_SIZES.iter().zip(&images) {
        file.write_all(&[if *size == 256 { 0 } else { *size as u8 }])?;
        file.write_all(&[if *size == 256 { 0 } else { *size as u8 }])?;
        file.write_all(&[0, 0])?;
        write_u16(&mut file, 1)?;
        write_u16(&mut file, 32)?;
        write_u32(&mut file, image.len() as u32)?;
        write_u32(&mut file, offset)?;
        offset += image.len() as u32;
    }
    for image in images {
        file.write_all(&image)?;
    }
    Ok(())
}

fn encode_bmp(size: u32) -> Vec<u8> {
    let pixels = render_icon(size);
    let mask_stride = size.div_ceil(32) * 4;
    let mut image = Vec::with_capacity((40 + size * size * 4 + mask_stride * size) as usize);
    image.extend_from_slice(&40u32.to_le_bytes());
    image.extend_from_slice(&(size as i32).to_le_bytes());
    image.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    image.extend_from_slice(&1u16.to_le_bytes());
    image.extend_from_slice(&32u16.to_le_bytes());
    image.extend_from_slice(&0u32.to_le_bytes());
    image.extend_from_slice(&(size * size * 4).to_le_bytes());
    image.extend_from_slice(&[0; 16]);
    for y in (0..size).rev() {
        for x in 0..size {
            let pixel = pixels[(y * size + x) as usize];
            image.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    image.resize(image.len() + (mask_stride * size) as usize, 0);
    image
}

fn render_icon(size: u32) -> Vec<[u8; 4]> {
    let mut pixels = vec![[0, 0, 0, 0]; (size * size) as usize];
    for y in 0..size {
        for x in 0..size {
            let px = x as f32 * 256.0 / size as f32;
            let py = y as f32 * 256.0 / size as f32;
            let pixel = &mut pixels[(y * size + x) as usize];
            if rounded_rect(px, py, 14.0, 14.0, 228.0, 228.0, 52.0) {
                *pixel = [15, 23, 42, 255];
            }
            if codex_mark(px, py) {
                *pixel = [67, 201, 122, 255];
            }
            if percent_mark(px, py) {
                *pixel = [248, 250, 252, 255];
            }
        }
    }
    pixels
}

fn rounded_rect(x: f32, y: f32, left: f32, top: f32, width: f32, height: f32, radius: f32) -> bool {
    let nearest_x = x.clamp(left + radius, left + width - radius);
    let nearest_y = y.clamp(top + radius, top + height - radius);
    (x - nearest_x).powi(2) + (y - nearest_y).powi(2) <= radius.powi(2)
}

fn codex_mark(x: f32, y: f32) -> bool {
    const CENTERS: &[(f32, f32)] = &[
        (202.0, 48.0), (220.0, 59.0), (220.0, 80.0),
        (202.0, 91.0), (184.0, 80.0), (184.0, 59.0),
    ];
    CENTERS.iter().any(|&(cx, cy)| ring(x, y, cx, cy, 15.0, 9.0))
}

fn percent_mark(x: f32, y: f32) -> bool {
    ring(x, y, 82.0, 103.0, 27.0, 14.0)
        || ring(x, y, 174.0, 193.0, 27.0, 14.0)
        || line(x, y, 179.0, 81.0, 77.0, 215.0, 15.0)
}

fn ring(x: f32, y: f32, cx: f32, cy: f32, radius: f32, width: f32) -> bool {
    let distance = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
    distance >= radius - width / 2.0 && distance <= radius + width / 2.0
}

fn line(x: f32, y: f32, start_x: f32, start_y: f32, end_x: f32, end_y: f32, width: f32) -> bool {
    let dx = end_x - start_x;
    let dy = end_y - start_y;
    let progress = (((x - start_x) * dx + (y - start_y) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    let closest_x = start_x + progress * dx;
    let closest_y = start_y + progress * dy;
    (x - closest_x).powi(2) + (y - closest_y).powi(2) <= (width / 2.0).powi(2)
}

fn write_u16(writer: &mut impl Write, value: u16) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}
