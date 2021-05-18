use std::cmp::min;
use std::process;

use crate::Error;
use crate::common::{calc_sr, calc_sr_u64, OGG_OPUS_SPS, MAX_NUM_CHANNELS};

use byteorder::{LittleEndian, ByteOrder};
use ogg::PacketWriter;
use magnum_opus::{Bitrate, Encoder as OpusEnc};
use rand::Rng;

const VER: &str = std::env!("CARGO_PKG_VERSION");

const fn to_samples<const S_PS: u32>(ms: u32) -> usize {
    ((S_PS * ms) / 1000) as usize
}


// In microseconds
const fn calc_fr_size(us: u32, channels:u8, sps:u32) -> usize {
    let samps_ms = (sps * us) as u32;
    const US_TO_MS: u32 = 10;
    ((samps_ms * channels as u32 ) / (1000 * US_TO_MS )) as usize
}


const fn opus_channels(val: u8) -> magnum_opus::Channels{
    if val == 0 {
        // Never should be 0
        magnum_opus::Channels::Mono
    }
    else if val == 1 {
       magnum_opus::Channels::Mono
    }
    else {
       magnum_opus::Channels::Stereo
    }
}

pub fn encode<const S_PS: u32>(audio: &[i16]) -> Result<(Vec<u8>, u32), Error> {
    //NOTE: In the future the S_PS const generic will let us use const on a lot 
    // of things, until then we need to use variables

    // This should have a bitrate of 24 Kb/s, exactly what IBM recommends

    // More frame time, sligtly less overhead more problematic packet loses,
    // a frame time of 20ms is considered good enough for most applications


    // Config
    const NUM_CHANNELS: u8 = 1;
    
    // Data
    const FRAME_TIME_MS: u32 = 20;
    const MAX_PACKET: usize = 4000; // Maximum theorical recommended by Opus
    const MIN_FRAME_MICROS: u32 = 25;

    let frame_samples: usize = to_samples::<S_PS>(FRAME_TIME_MS);
    let frame_size: usize = frame_samples * (NUM_CHANNELS as usize);

    // Generate the serial which is nothing but a value to identify a stream, we
    // will also use the process id so that two lily implementations don't use 
    // the same serial even if getting one at the same time
    let mut rnd = rand::thread_rng();
    let serial = rnd.gen::<u32>() ^ process::id();
    let mut buffer: Vec<u8> = Vec::new();
    
    let mut packet_writer = PacketWriter::new(&mut buffer);
    let mut opus_encoder = OpusEnc::new(S_PS, opus_channels(NUM_CHANNELS), magnum_opus::Application::Audio)?;
    opus_encoder.set_bitrate(Bitrate::Bits(24000))?;
    let skip = opus_encoder.get_lookahead().unwrap() as u16;
    let skip_us = skip as usize;
    let tot_samples = audio.len() + skip_us;
    let skip_48 = calc_sr(
        skip,
        S_PS,
        OGG_OPUS_SPS
    );

    let max = (tot_samples as f32 / frame_size as f32).floor() as u32;

    let calc = |counter: u32| -> usize {
        (counter as usize) * frame_size
    };

    let calc_samples = |counter:u32| -> usize {
        (counter as usize) * frame_samples
    };

    const fn granule<const S_PS: u32>(val: usize) -> u64 {
        calc_sr_u64(val as u64, S_PS, OGG_OPUS_SPS)
    }

    const OPUS_HEAD: [u8; 19] = [
        b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', // Magic header
        1, // Version number, always 1
        NUM_CHANNELS, // Channels
        0, 0,//Pre-skip
        0, 0, 0, 0, // Original Hz (informational)
        0, 0, // Output gain
        0, // Channel map family
        // If Channel map != 0, here should go channel mapping table
    ];

    fn encode_with_skip(opus_encoder: &mut OpusEnc, audio: &[i16], pos_a: usize, pos_b: usize, skip_us: usize) -> Result<Box<[u8]>, Error> {
        let res = if pos_a > skip_us {
            opus_encoder.encode_vec(&audio[pos_a-skip_us..pos_b-skip_us], MAX_PACKET)
        }
        else {
            let mut buf = Vec::with_capacity(pos_b-pos_a);
            buf.resize(pos_b-pos_a, 0);
            if pos_b > skip_us {
                buf[skip_us - pos_a..].copy_from_slice(&audio[.. pos_b - skip_us]);
            }
            opus_encoder.encode_vec(&buf, MAX_PACKET)
        };
        Ok(res?.into_boxed_slice())
    }

    fn is_end_of_stream(pos: usize, max: usize) -> ogg::PacketWriteEndInfo {
        if pos == max {
            ogg::PacketWriteEndInfo::EndStream
        }
        else {
            ogg::PacketWriteEndInfo::NormalPacket
        }
    }

    let mut head = OPUS_HEAD;
    LittleEndian::write_u16(&mut head[10..12], skip_48 as u16); // Write pre-skip
    LittleEndian::write_u32(&mut head[12..16], S_PS); // Write Samples per second

    let mut opus_tags : Vec<u8> = Vec::with_capacity(60);
    let vendor_str = format!("{}, ogg-opus {}", magnum_opus::version(), VER);
    opus_tags.extend(b"OpusTags");
    let mut len_bf = [0u8;4];
    LittleEndian::write_u32(&mut len_bf, vendor_str.len() as u32);
    opus_tags.extend(&len_bf);
    opus_tags.extend(vendor_str.bytes());
    opus_tags.extend(&[0]); // No user comments

    packet_writer.write_packet(Box::new(head), serial, ogg::PacketWriteEndInfo::EndPage, 0)?;
    packet_writer.write_packet(opus_tags.into_boxed_slice(), serial, ogg::PacketWriteEndInfo::EndPage, 0)?;

    // Do all frames
    for counter in 0..max{ // Last value of counter is max - 1 
        let pos_a: usize = calc(counter);
        let pos_b: usize = calc(counter + 1);
        
        assert!((pos_b - pos_a) <= frame_size);
        
        let new_buffer = encode_with_skip(&mut opus_encoder, audio, pos_a, pos_b, skip_us)?;

        packet_writer.write_packet(
            new_buffer,
            serial,
            is_end_of_stream(pos_b, tot_samples),
            granule::<S_PS>(skip_us + calc_samples(counter + 1)
        ))?;
    }

    // Calc the biggest frame buffer that still is either smaller or the
    // same size as the input
    fn calc_biggest_spills<T:PartialOrd + Copy>(val: T, possibles: &[T]) -> Option<T> {
        for container in possibles.iter().rev()  {
            if *container <= val {
                return Some(*container)
            }
        }
        None
    }

    fn encode_no_skip(opus_encoder: &mut OpusEnc, audio: &[i16], start: usize, frame_size : usize) -> Result<Box<[u8]>, Error> {
        let temp_buffer = opus_encoder.encode_vec(&audio[start .. start + frame_size], MAX_PACKET)?;
        Ok(temp_buffer.to_owned().into_boxed_slice())
    }

    // Try to add as less of empty audio as possible, first everything into
    // small frames, and on the last one, if needed fill with 0, since the
    // last one is going to be smaller this should be much less of a problem
    let mut last_sample = calc(max);
    assert!(last_sample <= audio.len() + skip_us);
    let frame_sizes: [usize; 4] = [
            calc_fr_size(MIN_FRAME_MICROS, NUM_CHANNELS, S_PS),
            calc_fr_size(50, NUM_CHANNELS, S_PS),
            calc_fr_size(100, NUM_CHANNELS, S_PS),
            calc_fr_size(200, NUM_CHANNELS, S_PS)
    ];

    while last_sample < tot_samples {

            let rem_samples = tot_samples - last_sample;
            let last_audio_s = last_sample - min(last_sample,skip_us);

            match calc_biggest_spills(rem_samples, &frame_sizes) {
                Some(frame_size) => {
                    let enc = if last_sample >= skip_us {
                        encode_no_skip(&mut opus_encoder, audio, last_audio_s, frame_size)?
                    }
                    else {
                        encode_with_skip(&mut opus_encoder, audio, last_sample, last_sample + frame_size, skip_us)?
                    };
                    last_sample += frame_size;
                    packet_writer.write_packet(
                        enc,
                        serial, 
                        is_end_of_stream(last_sample, tot_samples),
                        granule::<S_PS>(last_sample/(NUM_CHANNELS as usize))
                    )?;
                }
                None => {
                    // Maximum size for a 2.5 ms frame
                    const MAX_25_SIZE: usize = calc_fr_size(MIN_FRAME_MICROS, MAX_NUM_CHANNELS, OGG_OPUS_SPS);
                    let mut in_buffer = [0i16;MAX_25_SIZE];
                    let rem_skip = skip_us - min(last_sample, skip_us);
                    in_buffer[rem_skip..rem_samples].copy_from_slice(&audio[last_audio_s..]);

                    last_sample = tot_samples; // We end this here
                    
                    packet_writer.write_packet(
                        encode_no_skip(&mut opus_encoder, &in_buffer, 0, frame_sizes[0])?,
                        serial, 
                        ogg::PacketWriteEndInfo::EndStream,
                        granule::<S_PS>((skip_us + audio.len())/(NUM_CHANNELS as usize))
                    )?;
                    
                }
            }
            
        }

    let final_range = if cfg!(test) {opus_encoder.get_final_range()?}
                          else {0};

    Ok((buffer, final_range))
}