use std::{ffi::OsStr, fs::File, io::Write, path::Path};

use chrono::Local;
use rusb::{Context, DeviceHandle};
use util::{duration_of_rotation_as_stm_tim_raw, Density, DriveSelectState, DRIVE_SLOWEST_RPM};

use crate::{
    rawtrack::TrackFilter,
    track_parser::{amiga::AmigaTrackParser, c64::C64TrackParser, iso::IsoTrackParser},
    usb_commands::{configure_device, read_raw_track},
};

pub mod amiga;
pub mod c64;
pub mod iso;

pub struct TrackPayload {
    pub cylinder: u32,
    pub head: u32,
    pub payload: Vec<u8>,
}

pub struct CollectedSector {
    index: u32,
    payload: Vec<u8>,
}

pub trait TrackParser {
    fn parse_raw_track(&mut self, track: &[u8]) -> anyhow::Result<TrackPayload>;
    fn expect_track(&mut self, cylinder: u32, head: u32);
    fn step_size(&self) -> usize;
    fn track_density(&self) -> Density;
    fn duration_to_record(&self) -> usize;
    fn format_name(&self) -> &str;
    fn default_trackfilter(&self) -> TrackFilter;
    fn default_file_extension(&self) -> &str;
}

fn concatenate_sectors(
    mut collected_sectors: Vec<CollectedSector>,
    cylinder: u32,
    head: u32,
) -> TrackPayload {
    // Put the sectors in the right order before concatenating their data together
    collected_sectors.sort_by_key(|f| f.index);

    let mut track_data = Vec::with_capacity(collected_sectors.len() * 512);

    collected_sectors
        .iter_mut()
        .for_each(|f| track_data.append(&mut f.payload));

    TrackPayload {
        cylinder,
        head,
        payload: track_data,
    }
}

pub fn read_first_track_discover_format(
    usb_handles: &(DeviceHandle<Context>, u8, u8),
    select_drive: DriveSelectState,
) -> Option<Box<dyn TrackParser>> {
    // For some reason, the High density can read both densities on the first few cylinders...
    // This is very useful and I assume not random at all
    configure_device(&usb_handles, select_drive, Density::High, 0);

    // we need to make sure to read more than we need.
    // we only have one chance here. So just get 125% of the first track with the slowest drive we support.
    let duration_to_record = duration_of_rotation_as_stm_tim_raw(DRIVE_SLOWEST_RPM) * 125 / 100;

    let track_parsers: Vec<Box<dyn TrackParser>> = vec![
        Box::new(AmigaTrackParser::new(util::Density::SingleDouble)),
        Box::new(C64TrackParser::new()),
        Box::new(IsoTrackParser::new(None, Density::SingleDouble)),
        Box::new(IsoTrackParser::new(None, Density::High)),
    ];
    let cylinder = 0;
    let head = 0;

    let raw_data = read_raw_track(&usb_handles, cylinder, head, false, duration_to_record);

    let mut result: Option<Box<dyn TrackParser>> = None;

    for mut parser in track_parsers {
        parser.expect_track(cylinder, head);
        let track = parser.parse_raw_track(&raw_data).ok();

        if let Some(_track) = track {
            println!("Format is probably '{}'", parser.format_name());

            let old = result.replace(parser);
            if old.is_some() {
                println!("Warning: Multiple possible formats ?!?!?!?!")
            }
        }
    }

    result
}

pub fn read_tracks_to_diskimage(
    usb_handles: &(DeviceHandle<Context>, u8, u8),
    track_filter: Option<TrackFilter>,
    filepath: &str,
    select_drive: DriveSelectState,
) {
    let (mut track_parser, filepath) = if filepath == "justread" {
        let track_parser = read_first_track_discover_format(usb_handles, select_drive)
            .expect("Unable to detect floppy format!");

        let now = Local::now();
        let time_str = now.format("%Y%m%d_%H%M%S");
        let filepath = format!("{}.{}", time_str, track_parser.default_file_extension());

        println!("Resulting image will be {}", filepath);

        (track_parser, filepath)
    } else {
        let file_extension = Path::new(filepath)
            .extension()
            .and_then(OsStr::to_str)
            .expect("No file extension!");

        let track_parser: Box<dyn TrackParser> = match file_extension {
            "adf" => Box::new(AmigaTrackParser::new(util::Density::SingleDouble)),
            "d64" => Box::new(C64TrackParser::new()),
            "st" => Box::new(IsoTrackParser::new(None, Density::SingleDouble)),
            "img" => Box::new(IsoTrackParser::new(None, Density::High)),
            _ => panic!("{} is an unknown file extension!", file_extension),
        };

        (track_parser, filepath.into())
    };
    let track_filter = track_filter.unwrap_or(track_parser.default_trackfilter());

    let duration_to_record = track_parser.duration_to_record();
    configure_device(&usb_handles, select_drive, track_parser.track_density(), 0);

    let mut cylinder_begin = track_filter.cyl_start.unwrap_or(0);
    let mut cylinder_end = track_filter
        .cyl_end
        .expect("Please specify the last cylinder to read!");

    if cylinder_begin == cylinder_end {
        cylinder_begin = 0;
    } else {
        cylinder_end += 1;
    }

    let heads = match track_filter.head {
        Some(0) => 0..1,
        Some(1) => 1..2,
        None => 0..2,
        _ => panic!("Program flow error!"),
    };

    println!("Reading cylinders {} to {}", cylinder_begin, cylinder_end);
    let mut outfile = File::create(filepath).expect("Unable to create file");

    for cylinder in (cylinder_begin..cylinder_end).step_by(track_parser.step_size()) {
        for head in heads.clone() {
            track_parser.expect_track(cylinder, head);

            let mut possible_track: Option<TrackPayload> = None;

            for _ in 0..5 {
                let raw_data =
                    read_raw_track(&usb_handles, cylinder, head, false, duration_to_record);
                let track = track_parser.parse_raw_track(&raw_data).ok();

                if track.is_some() {
                    possible_track = track;
                    break;
                }

                println!(
                    "Reading of track {} {} not successful. Try again...",
                    cylinder, head
                )
            }

            let track =
                possible_track.expect(&format!("Unable to read track {} {}", cylinder, head));

            assert_eq!(cylinder, track.cylinder);
            assert_eq!(head, track.head);

            outfile.write_all(&track.payload).unwrap();
        }
    }
}
