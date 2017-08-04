use std::slice;
use syscall::error::Result;
use syscall::io::{Dma, Io};

mod capability;
mod command;
mod device;
mod doorbell;
mod event;
mod operational;
mod port;
mod runtime;
mod trb;

use self::capability::CapabilityRegs;
use self::command::CommandRing;
use self::device::DeviceList;
use self::doorbell::Doorbell;
use self::operational::OperationalRegs;
use self::port::Port;
use self::runtime::RuntimeRegs;

pub struct Xhci {
    cap: &'static mut CapabilityRegs,
    op: &'static mut OperationalRegs,
    ports: &'static mut [Port],
    dbs: &'static mut [Doorbell],
    run: &'static mut RuntimeRegs,
    devices: DeviceList,
    cmd: CommandRing,
}

impl Xhci {
    pub fn new(address: usize) -> Result<Xhci> {
        let cap = unsafe { &mut *(address as *mut CapabilityRegs) };
        println!("  - CAP {:X}", address);

        let op_base = address + cap.len.read() as usize;
        let op = unsafe { &mut *(op_base as *mut OperationalRegs) };
        println!("  - OP {:X}", op_base);

        let max_slots;
        let max_ports;

        {
            println!("  - Wait for ready");
            // Wait until controller is ready
            while op.usb_sts.readf(1 << 11) {
                println!("  - Waiting for XHCI ready");
            }

            println!("  - Stop");
            // Set run/stop to 0
            op.usb_cmd.writef(1, false);

            println!("  - Wait for not running");
            // Wait until controller not running
            while ! op.usb_sts.readf(1) {
                println!("  - Waiting for XHCI stopped");
            }

            println!("  - Reset");
            op.usb_cmd.writef(1 << 1, true);
            while op.usb_sts.readf(1 << 1) {
                println!("  - Waiting for XHCI reset");
            }

            println!("  - Read max slots");
            // Read maximum slots and ports
            let hcs_params1 = cap.hcs_params1.read();
            max_slots = (hcs_params1 & 0xFF) as u8;
            max_ports = ((hcs_params1 & 0xFF000000) >> 24) as u8;

            println!("  - Max Slots: {}, Max Ports {}", max_slots, max_ports);
        }

        let port_base = op_base + 0x400;
        let ports = unsafe { slice::from_raw_parts_mut(port_base as *mut Port, max_ports as usize) };
        println!("  - PORT {:X}", port_base);

        let db_base = address + cap.db_offset.read() as usize;
        let dbs = unsafe { slice::from_raw_parts_mut(db_base as *mut Doorbell, 256) };
        println!("  - DOORBELL {:X}", db_base);

        let run_base = address + cap.rts_offset.read() as usize;
        let run = unsafe { &mut *(run_base as *mut RuntimeRegs) };
        println!("  - RUNTIME {:X}", run_base);

        let mut xhci = Xhci {
            cap: cap,
            op: op,
            ports: ports,
            dbs: dbs,
            run: run,
            devices: DeviceList::new(max_slots)?,
            cmd: CommandRing::new()?,
        };

        xhci.init(max_slots);

        Ok(xhci)
    }

    pub fn init(&mut self, max_slots: u8) {
        // Set enabled slots
        println!("  - Set enabled slots to {}", max_slots);
        self.op.config.write(max_slots as u32);
        println!("  - Enabled Slots: {}", self.op.config.read() & 0xFF);

        // Set device context address array pointer
        println!("  - Write DCBAAP");
        self.op.dcbaap.write(self.devices.dcbaap());

        // Set command ring control register
        println!("  - Write CRCR");
        self.op.crcr.write(self.cmd.crcr());

        // Set event ring segment table registers
        println!("  - Interrupter 0: {:X}", self.run.ints.as_ptr() as usize);
        println!("  - Write ERSTZ");
        self.run.ints[0].erstsz.write(1);
        println!("  - Write ERDP");
        self.run.ints[0].erdp.write(self.cmd.events.trbs.physical() as u64);
        println!("  - Write ERSTBA: {:X}", self.cmd.events.ste.physical() as u64);
        self.run.ints[0].erstba.write(self.cmd.events.ste.physical() as u64);

        // Set run/stop to 1
        println!("  - Start");
        self.op.usb_cmd.writef(1, true);

        // Wait until controller is running
        println!("  - Wait for running");
        while self.op.usb_sts.readf(1) {
            println!("  - Waiting for XHCI running");
        }

        // Ring command doorbell
        println!("  - Ring doorbell");
        self.dbs[0].write(0);

        println!("  - XHCI initialized");
    }

    pub fn probe(&mut self) -> Result<()> {
        for (i, port) in self.ports.iter().enumerate() {
            let data = port.read();
            let state = port.state();
            let speed = port.speed();
            let flags = port.flags();
            println!("   + XHCI Port {}: {:X}, State {}, Speed {}, Flags {:?}", i, data, state, speed, flags);

            if flags.contains(port::PORT_CCS) {
                println!("  - Running Enable Slot command");

                let db = &mut self.dbs[0];
                let crcr = &mut self.op.crcr;
                let mut run = || {
                    db.write(0);
                    while crcr.readf(1 << 3) {
                        println!("  - Waiting for command completion");
                    }
                };

                {
                    let cmd = self.cmd.next_cmd();
                    cmd.enable_slot(0, true);
                    println!("  - Command: {}", cmd);

                    run();

                    cmd.reserved(false);
                }

                let slot;
                {
                    let event = self.cmd.next_event();
                    println!("  - Response: {}", event);
                    slot = (event.control.read() >> 24) as u8;

                    event.reserved(false);
                }

                println!(" Slot {}", slot);

                let mut trbs = Dma::<[trb::Trb; 256]>::zeroed()?;
                let mut input = Dma::<device::InputContext>::zeroed()?;
                {
                    input.add_context.write(1 << 1 | 1);

                    input.device.slot.a.write(1 << 27);
                    input.device.slot.b.write(((i as u32 + 1) & 0xFF) << 16);
                    println!("{:>08X}", input.device.slot.b.read());

                    input.device.endpoints[0].b.write(4096 << 16 | 4 << 3 | 3 << 1);
                    input.device.endpoints[0].trh.write((trbs.physical() >> 32) as u32);
                    input.device.endpoints[0].trl.write(trbs.physical() as u32 | 1);
                }

                {
                    let cmd = self.cmd.next_cmd();
                    cmd.address_device(slot, input.physical(), true);
                    println!("  - Command: {}", cmd);

                    run();

                    cmd.reserved(false);
                }

                let address;
                {
                    let event = self.cmd.next_event();
                    println!("  - Response: {}", event);
                    address = (event.control.read() >> 24) as u8;

                    event.reserved(false);
                }
            }
        }

        Ok(())
    }
}