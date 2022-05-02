#![cfg_attr(target_os = "none", no_std)]
#![cfg_attr(target_os = "none", no_main)]

mod api;

use api::*;
#[cfg(any(target_os = "none", target_os = "xous"))]
mod kbd;
#[cfg(any(target_os = "none", target_os = "xous"))]
use kbd::*;
#[cfg(any(target_os = "none", target_os = "xous"))]
mod hw;
#[cfg(any(target_os = "none", target_os = "xous"))]
use hw::*;
#[cfg(not(any(target_os = "none", target_os = "xous")))]
mod hosted;
#[cfg(not(any(target_os = "none", target_os = "xous")))]
use hosted::*;

use num_traits::*;
use xous::{CID, msg_scalar_unpack, Message, send_message};
use std::collections::BTreeMap;

#[xous::xous_main]
fn xmain() -> ! {
    use crate::SpinalUsbDevice;

    log_server::init_wait().unwrap();
    log::set_max_level(log::LevelFilter::Info);
    log::info!("my PID is {}", xous::process::id());

    let xns = xous_names::XousNames::new().unwrap();
    let usbtest_sid = xns.register_name(api::SERVER_NAME_USBTEST, None).expect("can't register server");
    log::trace!("registered with NS -- {:?}", usbtest_sid);

    let mut usbtest = SpinalUsbDevice::new(usbtest_sid);
    let mut kbd = Keyboard::new(usbtest_sid);

    log::trace!("ready to accept requests");

    std::thread::spawn({
        move || {
            let tt = ticktimer_server::Ticktimer::new().unwrap();
            let mut keepalive = 0;
            loop {
                tt.sleep_ms(2500).unwrap();
                log::info!("keepalive {}", keepalive);
                keepalive += 1;
            }
        }
    });

    // register a suspend/resume listener
    let cid = xous::connect(usbtest_sid).expect("couldn't create suspend callback connection");
    let mut susres = susres::Susres::new(
        None,
        &xns,
        api::Opcode::SuspendResume as u32,
        cid
    ).expect("couldn't create suspend/resume object");

    let mut cmdline = String::new();
    loop {
        let msg = xous::receive_message(usbtest_sid).unwrap();
        match FromPrimitive::from_usize(msg.body.id()) {
            Some(Opcode::SuspendResume) => xous::msg_scalar_unpack!(msg, token, _, _, _, {
                kbd.suspend();
                usbtest.suspend();
                susres.suspend_until_resume(token).expect("couldn't execute suspend/resume");
                kbd.resume();
                usbtest.resume();
            }),
            Some(Opcode::DoCmd) => {
                log::info!("got command line: {}", cmdline);
                if let Some((cmd, args)) = cmdline.split_once(' ') {
                    // command and args
                    match cmd {
                        "test" => {
                            log::info!("got test command with arg {}", args);
                        }
                        "conn" => {
                            match args {
                                "1" => usbtest.connect_device_core(true),
                                "0" => usbtest.connect_device_core(false),
                                _ => log::info!("usage: conn [1,0]; got: 'conn {}'", args),
                            }
                        }
                        _ => {
                            log::info!("unrecognied command {}", cmd);
                        }
                    }
                } else {
                    // just the command
                    match cmdline.as_str() {
                        "help" => {
                            log::info!("wouldn't that be nice...");
                        }
                        "conn" => {
                            usbtest.connect_device_core(true);
                            log::info!("device core connected");
                            usbtest.print_regs();
                        }
                        "regs" => {
                            usbtest.print_regs();
                        }
                        _ => {
                            log::info!("unrecognized command");
                        }
                    }
                }
                cmdline.clear();
            }
            // this is via UART
            Some(Opcode::KeyboardChar) => msg_scalar_unpack!(msg, k, _, _, _, {
                let key = {
                    let bs_del_fix = if k == 0x7f {
                        0x08
                    } else {
                        k
                    };
                    core::char::from_u32(bs_del_fix as u32).unwrap_or('\u{0000}')
                };
                if key != '\u{0000}' {
                    if key != '\u{000d}' {
                        cmdline.push(key);
                    } else {
                        send_message(cid, Message::new_scalar(
                            Opcode::DoCmd.to_usize().unwrap(), 0, 0, 0, 0
                        )).unwrap();
                    }
                }
            }),
            // this is via physical keyboard
            Some(Opcode::HandlerTrigger) => {
                let rawstates = kbd.update();
                // interpret scancodes
                let kc: Vec<char> = kbd.track_keys(&rawstates);
                // handle keys, if any
                for &key in kc.iter() {
                    if key != '\u{000d}' {
                        cmdline.push(key);
                    } else {
                        send_message(cid, Message::new_scalar(
                            Opcode::DoCmd.to_usize().unwrap(), 0, 0, 0, 0
                        )).unwrap();
                    }
                }
            },
            Some(Opcode::Quit) => {
                log::warn!("Quit received, goodbye world!");
                break;
            },
            None => {
                log::error!("couldn't convert opcode: {:?}", msg);
            }
        }
    }
    // clean up our program
    log::trace!("main loop exit, destroying servers");
    xns.unregister_server(usbtest_sid).unwrap();
    xous::destroy_server(usbtest_sid).unwrap();
    log::trace!("quitting");
    xous::terminate_process(0)
}

pub(crate) const START_OFFSET: u32 = 0x0048 + 8; // align spinal free space to 16-byte boundary
pub(crate) const END_OFFSET: u32 = 0xFF00;
/// USB endpoint allocator. The SpinalHDL USB controller appears as a block of
/// unstructured memory to the host. You can specify pointers into the memory with
/// an offset and length to define where various USB descriptors should be placed.
/// This allocator manages that space.
///
/// Note that all allocations must be aligned to 16-byte boundaries. This is a restriction
/// of the USB core.
pub(crate) fn alloc_inner(allocs: &mut BTreeMap<u32, u32>, requested: u32) -> Option<u32> {
    if requested == 0 {
        return None;
    }
    let mut alloc_offset = START_OFFSET;
    for (&offset, &length) in allocs.iter() {
        // round length up to the nearest 16-byte increment
        let length = if length & 0xF == 0 { length } else { (length + 16) & !0xF };
        // println!("aoff: {}, cur: {}+{}", alloc_offset, offset, length);
        assert!(offset >= alloc_offset, "allocated regions overlap");
        if offset > alloc_offset {
            if offset - alloc_offset >= requested {
                // there's a hole in the list, insert the element here
                break;
            }
        }
        alloc_offset = offset + length;
    }
    if alloc_offset + requested <= END_OFFSET {
        allocs.insert(alloc_offset, requested);
        Some(alloc_offset)
    } else {
        None
    }
}
pub(crate) fn dealloc_inner(allocs: &mut BTreeMap<u32, u32>, offset: u32) -> bool {
    allocs.remove(&offset).is_some()
}

// run with `cargo test -- --nocapture --test-threads=1`:
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_alloc() {
        use rand_chacha::ChaCha8Rng;
        use rand_chacha::rand_core::SeedableRng;
        use rand_chacha::rand_core::RngCore;
        let mut rng = ChaCha8Rng::seed_from_u64(0);

        let mut allocs = BTreeMap::<u32, u32>::new();
        assert_eq!(alloc_inner(&mut allocs, 128), Some(START_OFFSET));
        assert_eq!(alloc_inner(&mut allocs, 64), Some(START_OFFSET + 128));
        assert_eq!(alloc_inner(&mut allocs, 256), Some(START_OFFSET + 128 + 64));
        assert_eq!(alloc_inner(&mut allocs, 128), Some(START_OFFSET + 128 + 64 + 256));
        assert_eq!(alloc_inner(&mut allocs, 128), Some(START_OFFSET + 128 + 64 + 256 + 128));
        assert_eq!(alloc_inner(&mut allocs, 128), Some(START_OFFSET + 128 + 64 + 256 + 128 + 128));
        assert_eq!(alloc_inner(&mut allocs, 0xFF00), None);

        // create two holes and fill first hole, interleaved
        assert_eq!(dealloc_inner(&mut allocs, START_OFFSET + 128 + 64), true);
        let mut last_alloc = 0;
        // consistency check and print out
        for (&offset, &len) in allocs.iter() {
            assert!(offset >= last_alloc, "new offset is inside last allocation!");
            println!("{}-{}", offset, offset+len);
            last_alloc = offset + len;
        }

        assert_eq!(alloc_inner(&mut allocs, 128), Some(START_OFFSET + 128 + 64));
        assert_eq!(dealloc_inner(&mut allocs, START_OFFSET + 128 + 64 + 256 + 128), true);
        assert_eq!(alloc_inner(&mut allocs, 128), Some(START_OFFSET + 128 + 64 + 128));

        // alloc something that doesn't fit at all
        assert_eq!(alloc_inner(&mut allocs, 256), Some(START_OFFSET + 128 + 64 + 256 + 128 + 128 + 128));

        // fill second hole
        assert_eq!(alloc_inner(&mut allocs, 128), Some(START_OFFSET + 128 + 64 + 256 + 128));

        // final tail alloc
        assert_eq!(alloc_inner(&mut allocs, 64), Some(START_OFFSET + 128 + 64 + 256 + 128 + 128 + 128 + 256));

        println!("after structured test:");
        let mut last_alloc = 0;
        // consistency check and print out
        for (&offset, &len) in allocs.iter() {
            assert!(offset >= last_alloc, "new offset is inside last allocation!");
            println!("{}-{}({})", offset, offset+len, len);
            last_alloc = offset + len;
        }

        // random alloc/dealloc and check for overlapping regions
        let mut tracker = Vec::<u32>::new();
        for _ in 0..10240 {
            if rng.next_u32() % 2 == 0 {
                if tracker.len() > 0 {
                    //println!("tracker: {:?}", tracker);
                    let index = tracker.remove((rng.next_u32() % tracker.len() as u32) as usize);
                    //println!("removing: {} of {}", index, tracker.len());
                    assert_eq!(dealloc_inner(&mut allocs, index), true);
                }
            } else {
                let req = rng.next_u32() % 256;
                if let Some(offset) = alloc_inner(&mut allocs, req) {
                    //println!("tracker: {:?}", tracker);
                    //println!("alloc: {}+{}", offset, req);
                    tracker.push(offset);
                }
            }
        }

        let mut last_alloc = 0;
        // consistency check and print out
        println!("after random test:");
        for (&offset, &len) in allocs.iter() {
            assert!(offset >= last_alloc, "new offset is inside last allocation!");
            assert!(offset & 0xF == 0, "misaligned allocation detected");
            println!("{}-{}({})", offset, offset+len, len);
            last_alloc = offset + len;
        }
    }
}