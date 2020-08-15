extern crate alloc;
extern crate multiwii_serial_protocol_v2;
extern crate serialport;
extern crate packed_struct;

use multiwii_serial_protocol_v2::{MspPacket, MspParser};
use serialport::SerialPort;

use async_std::sync::{channel, Arc, Condvar, Mutex, Sender, Receiver};
use async_std::{io, task};

use std::time::Duration;
use std::sync::atomic::{AtomicBool, Ordering};


#[derive(Clone)]
pub struct Core {
    parser_locked: Arc<Mutex<MspParser>>,

    msp_reader_send: Sender<MspPacket>,
    msp_reader_recv: Receiver<MspPacket>,
    msp_writer_send: Sender<MspPacket>,
    msp_writer_recv: Receiver<MspPacket>,
}

impl Core {
    /// Create new core msp reader and parser
    pub fn new() -> Core {
        let (msp_reader_send, msp_reader_recv) = channel::<MspPacket>(4096);
        let (msp_writer_send, msp_writer_recv) = channel::<MspPacket>(1024);

        let parser = MspParser::new();
        let parser_locked = Arc::new(Mutex::new(parser));

        return Core {
            parser_locked: parser_locked,
            msp_reader_send: msp_reader_send,
            msp_reader_recv: msp_reader_recv,
            msp_writer_send: msp_writer_send,
            msp_writer_recv: msp_writer_recv,
        };
	  }

    pub fn start(&self, serial: Box<dyn SerialPort>, msp_write_delay: Duration, buffer_size: usize) {
        serial.clear(serialport::ClearBuffer::All).unwrap();
        let serial_clone = serial.try_clone().unwrap();
        let serial_write_lock = Arc::new((Mutex::new(buffer_size), Condvar::new()));
        let serial_write_lock_clone = serial_write_lock.clone();

        Core::process_input(serial, self.parser_locked.clone(), self.msp_reader_send.clone(), serial_write_lock);
        Core::process_output(serial_clone, self.msp_writer_recv.clone(), msp_write_delay, serial_write_lock_clone);
    }

    pub async fn read(&self) -> std::option::Option<MspPacket> {
        return match self.msp_reader_recv.recv().await {
            Err(_) => None,
            Ok(packet) => Some(packet),
        };
    }

    pub async fn write(&self, packet: MspPacket)  {
        self.msp_writer_send.send(packet).await;
    }

    // TODO: return joinhandler, so we can stop the tasks on drop
    // TODO: rewrite using stream api with inspect, each command will inspect
    //       and passthorugh to next.
    //       if the stream contained response for command, it will return the read/write function
    fn process_input(
        mut serial: Box<dyn SerialPort>,
        parser_locked: Arc<Mutex<MspParser>>,
        msp_reader_send: Sender<MspPacket>,
        serial_write_lock: Arc<(Mutex<usize>, Condvar)>,
    ) -> Arc<AtomicBool> {
        // TODO: remove the should stop, once this object gets dropped, this will stop
        let should_stop = Arc::new(AtomicBool::new(false));
        let should_stop_clone = should_stop.clone();

        // task 1: read into input channel from serial(reading from serial is blocking)
        task::spawn(async move {
            let (lock, cvar) = &*serial_write_lock;
            let initial_lock = lock.lock().await;
            let initial_buffer_size = *initial_lock;
            drop(initial_lock);

            while should_stop.load(Ordering::Relaxed) == false {
                let mut serial_buf: Vec<u8> = vec![0; 0x1000];
                match serial.read(serial_buf.as_mut_slice()) {
                    Ok(bytes) => {
                        // println!("bytes: {}", bytes);
                        let mut parser = parser_locked.lock().await;
                        for n in 0..bytes {
                            let res = parser.parse(serial_buf[n]);
                            match res {
                                Ok(Some(p)) => {
                                    // println!("reading");
                                    msp_reader_send.send(p).await;

                                    // lock the condvar here and update to true, and decrement the sent packets count
                                    let mut received_lock = lock.lock().await;
                                    if *received_lock < initial_buffer_size {
                                        *received_lock += 1;
                                        // We notify the condvar that the value has changed.
                                        cvar.notify_one();
                                    }
                                },
                                Err(e) => eprintln!("bad crc {:?}", e),
                                Ok(None) => ()
                            }
                        }
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::TimedOut => {
                        // println!("read timeout");
                    }
                    Err(e) => eprintln!("{:?}", e),
                }

                task::yield_now().await;
            }
        });
        return should_stop_clone;
	  }

    // TODO: return joinhandler, so we can stop the tasks on drop
    fn process_output(
        mut serial: Box<dyn SerialPort>,
        msp_writer_recv: Receiver<MspPacket>,
        write_delay: Duration,
        serial_write_lock: Arc<(Mutex<usize>, Condvar)>,
    ) {
        task::spawn(async move {
            let (lock, cvar) = &*serial_write_lock;

            loop {
                // lock here counter for sent packets
                // if counter is more then buffer size(10), lock then 10 turn the value to false and continue the loop
                // essentially waiting for value to change
                let guard = cvar.wait_until(lock.lock().await, |send_count| {
                    if *send_count > 0 {
                        *send_count -=1;
                        return true;
                    }

                    return false;
                }).await;
                drop(guard);
                let packet = match msp_writer_recv.recv().await {
                    Err(_) => break,
                    Ok(packet) => packet,
                };

                let size = packet.packet_size_bytes_v2();
                let mut output = vec![0; size];

                packet
                    .serialize_v2(&mut output)
                    .expect("Failed to serialize");

                // println!("writing {:?}", packet);
                // because inav doesn't support uart flow control, we simply try write untill success
                loop {
                    match serial.write(&output) {
                        Ok(_) => break,
                        Err(ref e) if e.kind() == io::ErrorKind::TimedOut => {
                            // controller is busy/serial buffer is full, sleep and attempt write again
                            // println!("write timeout, retrying");
                            task::yield_now().await;
                        }
                        Err(e) => {
                            *(lock.lock().await) += 1;
                            eprintln!("failed to write{:?}", e);
                        }
                    }
                }

                if write_delay > Duration::from_millis(0) {
                    task::sleep(write_delay).await;
                }

                task::yield_now().await;
            }
        });
	  }

    pub async fn reset_parser(&self) {
        (*self.parser_locked.lock().await).reset();
    }
}

// impl Clone for Core {
//     fn clone(&self) -> Self {
//         return Core {
//             parser_locked: self.parser_locked.clone(),
//             msp_reader_send: self.msp_reader_send.clone(),
//             msp_reader_recv: self.msp_reader_recv.clone(),
//             msp_writer_send: self.msp_writer_send.clone(),
//             msp_writer_recv: self.msp_writer_recv.clone(),
//         };
//     }
// }
