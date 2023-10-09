use anyhow::{anyhow, Context, Result};
use blake3::hash;
use clap::Parser;
use fast_image_resize as fr;
use image::io::Reader as ImageReader;
use image::RgbImage;
use indicatif::ParallelProgressIterator;
use indicatif::{ProgressBar, ProgressStyle};
use log::debug;
use nom::branch::alt;
use nom::character::complete::{char, digit1, multispace0};
use nom::combinator::{map, map_res, opt};
use nom::multi::separated_list0;
use nom::sequence::{preceded, separated_pair, terminated, tuple};
use nom::IResult;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use simplelog::SharedLogger;
use std::cmp::Ordering;
use std::io::BufRead;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::{fs, io};

fn parse_u32(input: &str) -> IResult<&str, u32> {
    map_res(digit1, |digit_str: &str| digit_str.parse::<u32>())(input)
}

fn parse_bidirectional_resolution(input: &str) -> IResult<&str, Vec<(u32, u32)>> {
    map(
        preceded(char('['), terminated(parse_sizes, char(']'))),
        |res| {
            res.iter()
                .flat_map(|&(width, height)| vec![(width, height), (height, width)])
                .collect()
        },
    )(input)
}

fn parse_resolution(input: &str) -> IResult<&str, Vec<(u32, u32)>> {
    alt((parse_sizes, parse_bidirectional_resolution))(input)
}

fn parse_range(input: &str) -> IResult<&str, Vec<u32>> {
    let (input, (start, _, end, step)) = tuple((
        parse_u32,
        char(':'),
        parse_u32,
        opt(preceded(char(':'), parse_u32)),
    ))(input)?;
    Ok((input, generate_values((start, end, step.unwrap_or(1)))))
}

fn generate_values((start, end, step): (u32, u32, u32)) -> Vec<u32> {
    (start..=end).step_by(step as usize).collect()
}

fn parse_size(input: &str) -> IResult<&str, Vec<u32>> {
    alt((parse_range, map(parse_u32, |size| vec![size])))(input)
}

fn parse_sizes(input: &str) -> IResult<&str, Vec<(u32, u32)>> {
    let (input, (widths, heights)) = alt((
        separated_pair(parse_size, char('x'), parse_size),
        map(parse_size, |sizes| (sizes.clone(), sizes.clone())),
    ))(input)?;

    Ok((
        input,
        widths
            .iter()
            .flat_map(|w| heights.iter().map(|h| (*w, *h)))
            .collect(),
    ))
}

fn parse_resolutions(input: &str) -> IResult<&str, Vec<(u32, u32)>> {
    let (input, res_list) =
        separated_list0(terminated(char(','), multispace0), parse_resolution)(input)?;
    Ok((input, res_list.into_iter().flatten().collect()))
}

#[derive(Parser, Debug)]
#[command(long_about = "
Crop all your images with snapping

Examples:

Crop images to SDXL training resolutions
    snapcrop out --res 1024x1024,1152x896,896x1152,1216x832,832x1216,1344x768,768x1344,1536x640,640x1536

Crop images to the closest resolution of the provided 1:1 sizes
    snapcrop out --res 1024,768,512

Crop images to the closest resolution of 1:1 aspect ratio between 512x512 and 1024x1024 with a step of 64
    snapcrop out --res 512:1024:64

Crop images with a fixed width of 512 and a height between 1024 and 512 with a step of 64
    snapcrop out --res 512:1024:64x512

Crop images to resolution in either orientation (512x768 and 768x512)
    snapcrop out --res [512x768]

Combine freely
    snapcrop out --res [512x768],1024,512:768:64x768:1024:32
")]
struct Args {
    /// Output dir path for images
    output_path: PathBuf,

    /// List of resolutions
    #[arg(long = "res")]
    resolutions: String,

    /// File list path, stdin used otherwise
    #[arg(short = 'i', long = "input-file")]
    image_list_path: Option<PathBuf>,

    /// Output debug info to log
    #[arg(short = 'l', long = "log")]
    log_path: Option<PathBuf>,

    /// Output debug info to stderr
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    // Image format to save files with
    #[arg(short = 'f', long = "format", default_value = "png")]
    image_format: String,
}

fn resize_image(
    src_view: fr::DynamicImageView,
    resize_w: NonZeroU32,
    resize_h: NonZeroU32,
) -> Result<fr::Image> {
    let mut dst_image = fr::Image::new(resize_w, resize_h, src_view.pixel_type());
    let mut dst_view = dst_image.view_mut();
    let mut resizer = fr::Resizer::new(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3));
    resizer.resize(&src_view, &mut dst_view)?;
    Ok(dst_image)
}

fn crop_image(
    mut src_view: fr::DynamicImageView,
    crop_w: NonZeroU32,
    crop_h: NonZeroU32,
) -> Result<fr::Image> {
    src_view.set_crop_box_to_fit_dst_size(crop_w, crop_h, None);
    let mut dst_image = fr::Image::new(crop_w, crop_h, src_view.pixel_type());
    let mut dst_view = dst_image.view_mut();
    let mut resizer = fr::Resizer::new(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3));
    resizer.resize(&src_view, &mut dst_view)?;
    Ok(dst_image)
}

fn resize_and_crop(src_view: fr::DynamicImageView, res: Vec<(u32, u32)>) -> Result<fr::Image> {
    // Calculate aspect ratio of the image
    let img_w = src_view.width().get();
    let img_h = src_view.height().get();
    let img_ratio = img_w as f64 / img_h as f64;

    // Find the resolution with the closest aspect ratio
    let &(valid_w, valid_h) = res
        .iter()
        .min_by(|&&(w1, h1), &&(w2, h2)| {
            let ratio1 = (img_ratio - w1 as f64 / h1 as f64).abs();
            let ratio2 = (img_ratio - w2 as f64 / h2 as f64).abs();
            ratio1.partial_cmp(&ratio2).unwrap_or(Ordering::Equal)
        })
        .ok_or_else(|| anyhow!("Could not find a valid resolution target"))?;

    println!("{}x{} -> {}x{}", img_w, img_h, valid_w, valid_h);

    if img_w < valid_w || img_h < valid_h {
        return Err(anyhow!(
            "Image too small, skipping: {}x{} < {}x{}",
            img_w,
            img_h,
            valid_w,
            valid_h
        ));
    }

    let (resize_w, resize_h) = if img_ratio > valid_w as f64 / valid_h as f64 {
        // If the image is more "landscape" than the target, match its height to the target height
        ((img_w as f64 * valid_h as f64 / img_h as f64).round() as u32, valid_h)
    } else {
        // If the image is more "portrait" or equal to the target, match its width to the target width
        (valid_w, (valid_w as f64 * img_h as f64 / img_w as f64).round() as u32)
    };

    // Resize the image while maintaining its original aspect ratio
    let resized_image = resize_image(
        src_view,
        NonZeroU32::new(resize_w).ok_or_else(|| anyhow!("Invalid resize width"))?,
        NonZeroU32::new(resize_h).ok_or_else(|| anyhow!("Invalid resize height"))?,
    )
    .with_context(|| "Failed to resize image")?;

    // Crop the resized image to the exact dimensions of the chosen valid resolution
    let cropped_image = crop_image(
        resized_image.view(),
        NonZeroU32::new(valid_w).ok_or_else(|| anyhow!("Invalid target width"))?,
        NonZeroU32::new(valid_h).ok_or_else(|| anyhow!("Invalid target height"))?,
    )
    .with_context(|| "Failed to crop image")?;

    Ok(cropped_image.copy())
}

fn save_image(image: &fr::Image, path: &Path) -> Result<()> {
    let width = image.width().get() as u32;
    let height = image.height().get() as u32;
    let buffer = image.buffer().to_vec();

    let img = RgbImage::from_raw(width, height, buffer)
        .with_context(|| "Failed to convert to RgbImage")?;

    img.save(path).with_context(|| "Failed to save the image")
}

fn process_image(
    path: &Path,
    output_path: &Path,
    image_format: &str,
    res: Vec<(u32, u32)>,
) -> Result<()> {
    let data = fs::read(path).context("Failed to read image file")?;
    let image_name = format!("{}.{}", hash(&data).to_hex(), image_format);
    let output_image_path = output_path.join(&image_name);

    if output_image_path.exists() {
        return Err(anyhow!(
            "Image already exists in output dir, skipping: {}",
            image_name
        ));
    }

    let img = ImageReader::open(path)
        .with_context(|| format!("Failed to open image from path: {}", path.display()))?
        .with_guessed_format()?
        .decode()
        .context("Failed to decode image")?;

    let width = NonZeroU32::new(img.width()).ok_or_else(|| anyhow!("Invalid image width"))?;
    let height = NonZeroU32::new(img.height()).ok_or_else(|| anyhow!("Invalid image height"))?;

    let src_image =
        fr::Image::from_vec_u8(width, height, img.to_rgb8().into_raw(), fr::PixelType::U8x3)
            .context("Failed to create image from vector")?;

    let resized_cropped_image = resize_and_crop(src_image.view(), res);

    save_image(&resized_cropped_image?, &output_image_path)?;

    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    let mut loggers: Vec<Box<dyn SharedLogger>> = vec![];

    if let Some(log_path) = args.log_path {
        loggers.push(simplelog::WriteLogger::new(
            simplelog::LevelFilter::Debug,
            simplelog::Config::default(),
            std::fs::File::create(log_path)?,
        ));
    }

    if args.verbose {
        loggers.push(simplelog::TermLogger::new(
            simplelog::LevelFilter::Debug,
            simplelog::Config::default(),
            simplelog::TerminalMode::Stderr,
            simplelog::ColorChoice::Auto,
        ));
    }

    simplelog::CombinedLogger::init(loggers).context("Failed to initialize logger")?;

    let mut res: Vec<(u32, u32)> = parse_resolutions(&args.resolutions)
        .map_err(|e| anyhow!("Failed to parse resolutions: {}", e))
        .and_then(|(_, res)| Ok(res))?;

    res.sort();
    res.dedup();
    res.reverse();

    // Ensure res is not empty and no dimension is 0
    if res.is_empty() || res.iter().any(|&(w, h)| w == 0 || h == 0) {
        return Err(anyhow!("Invalid resolutions, {:?}", res));
    }

    debug!("Resolutions: {:?}", res);

    let image_paths: Vec<String> = match args.image_list_path {
        Some(image_list_path) => fs::read_to_string(image_list_path)
            .with_context(|| "Failed to read image list file")?
            .lines()
            .map(|line| line.to_owned())
            .collect(),
        None => io::stdin().lock().lines().filter_map(Result::ok).collect(),
    };

    let pb = ProgressBar::new(image_paths.len() as u64);

    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                " {spinner:.green} [{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} ({eta}) {msg}",
            )?
            .progress_chars("**-"),
    );

    image_paths.par_iter().progress_with(pb).for_each(|path| {
        if let Err(e) = process_image(
            Path::new(&path),
            &args.output_path,
            &args.image_format,
            res.to_owned(),
        ) {
            debug!("{}: {}", path, e);
        }
    });

    Ok(())
}
