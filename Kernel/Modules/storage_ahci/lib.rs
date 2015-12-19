// "Tifflin" Kernel - AHCI (SATA) Driver
// - By John Hodge (thePowersGang)
//
// Modules/storage_ahci/lib.rs
//! AHCI Driver
#![feature(linkage)]
#![feature(associated_consts)]
#![feature(clone_from_slice)]
#![no_std]

use kernel::prelude::*;
use kernel::device_manager;
use kernel::memory::virt::AllocHandle;
use kernel::lib::mem::aref::{ArefBorrow,ArefInner};
use kernel::sync::EventChannel;
use core::sync::atomic::Ordering;
use kernel::sync::atomic::AtomicU32;
use kernel::metadevs::storage;

#[macro_use]
extern crate kernel;

extern crate storage_ata;

module_define!{AHCI, [DeviceManager, Storage], init}

mod bus_bindings;
mod hw;

fn init()
{
	device_manager::register_driver(&bus_bindings::S_PCI_DRIVER);
}

/// ACHI Controller
struct Controller
{
	inner: ArefInner<ControllerInner>,
	ports: Vec<Port>,
	irq_handle: Option<::kernel::irqs::ObjectHandle>,
}
struct ControllerInner
{
	io_base: device_manager::IOBinding,
	max_commands: u8,
	supports_64bit: bool,
}
struct Port
{
	index: usize,
	ctrlr: ArefBorrow<ControllerInner>,
	
	// Hardware allocations:
	// - 1KB (<32*32 bytes) for the command list
	// - 256 bytes of Received FIS
	// - <16KB (32*256 bytes) of command tables
	// Contains the "Command List" (a 1KB aligned block of memory containing commands)
	command_list_alloc: AllocHandle,
	command_tables: [AllocHandle; 4],

	command_events: Vec<::kernel::sync::EventChannel>,

	used_commands_sem: ::kernel::sync::Semaphore,
	used_commands: AtomicU32,

	issued_commands_bs: u32,	// Bitset, see also PxSACT
	
}
struct PortRegs<'a>
{
	idx: usize,
	io: &'a device_manager::IOBinding,
}

enum DataPtr<'a>
{
	Send(&'a [u8]),
	Recv(&'a mut [u8]),
}
impl<'a> DataPtr<'a> {
	fn as_slice(&self) -> &[u8] {
		match self
		{
		&DataPtr::Send(p) => p,
		&DataPtr::Recv(ref p) => p,
		}
	}
	fn is_send(&self) -> bool {
		match self
		{
		&DataPtr::Send(_) => true,
		&DataPtr::Recv(_) => false,
		}
	}
}
impl<'a> ::core::fmt::Debug for DataPtr<'a> {
	fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
		match self
		{
		&DataPtr::Send(p) => write!(f, "Send({:p}+{})", p.as_ptr(), p.len()),
		&DataPtr::Recv(ref p) => write!(f, "Recv(mut {:p}+{})", p.as_ptr(), p.len()),
		}
	}
}

impl Controller
{
	pub fn new(irq: u32, io: device_manager::IOBinding) -> Result<Box<Controller>, device_manager::DriverBindError>
	{

		// Enumerate implemented ports
		let ports_implemented;
		// SAFE: Enumerate access to hardware
		let (n_ports, max_commands, supports_64bit) = unsafe {
			io.write_32(hw::REG_GHC, hw::GHC_AE);
			ports_implemented = io.read_32(hw::REG_PI);
			
			// Enumerate ports, returning all to idle if needed
			let mut n_ports = 0;
			for port_idx in 0 .. 32
			{
				if ports_implemented & (1 << port_idx) == 0 {
					continue ;
				}
				let port = PortRegs::new(&io, port_idx);
				
				// If the port is not idle, then tell it to go idle
				let cmd = port.read(hw::REG_PxCMD);
				if cmd & (hw::PxCMD_ST|hw::PxCMD_CR|hw::PxCMD_FRE|hw::PxCMD_FR) != 0 {
					port.write(hw::REG_PxCMD, 0);
				}

				n_ports += 1;
			}

			let capabilities = io.read_32(hw::REG_CAP);
			let supports_64bit = capabilities & hw::CAP_S64A != 0;
			let max_commands = ((capabilities & hw::CAP_NCS) >> hw::CAP_NCS_ofs) + 1;
			
			(n_ports, max_commands, supports_64bit,)
			};
		
		// Construct controller structure
		let mut ret = Box::new( Controller {
			// SAFE: The inner is boxed (and hence gets a fixed address) before it's borrowed
			inner: unsafe {ArefInner::new(ControllerInner {
				io_base: io,
				supports_64bit: supports_64bit,
				max_commands: max_commands as u8,
				}) },
			ports: Vec::with_capacity(n_ports),
			irq_handle: None,
			});
		
		// Allocate port information
		for port_idx in 0 .. 32
		{
			let mask = 1 << port_idx;
			if ports_implemented & mask == 0 {
				continue ;
			}


			{
				let port = PortRegs::new(&ret.inner.io_base, port_idx);
			
				let cmd = port.read(hw::REG_PxCMD);
				if cmd & (hw::PxCMD_CR|hw::PxCMD_FR) != 0 {
					todo!("AHCI Init: Wait for ports to become idle");
				}
			}

			let port = try!(Port::new(ret.inner.borrow(), port_idx, max_commands as usize));
			ret.ports.push( port );
		}

		// Enable interrupts
		// SAFE: Exclusive access to these registers
		unsafe {
			ret.inner.io_base.write_32(hw::REG_IS, !0);
			// TODO: What else shoud be set?
			ret.inner.io_base.write_32(hw::REG_GHC, hw::GHC_AE|hw::GHC_IE);
		}
		
		// Bind interrupt
		{
			struct RawSend<T: Send>(*const T);
			unsafe impl<T: Send> Send for RawSend<T> {}
			let ret_raw = RawSend(&*ret);
			// SAFE: Pointer _should_ be valid as long as this IRQ binding exists
			ret.irq_handle = Some(::kernel::irqs::bind_object(irq, Box::new(move || unsafe { (*ret_raw.0).handle_irq() } )));
		}

		// Update port status once fully populated
		for port in &ret.ports
		{
			port.update_connection();
		}

		Ok( ret )
	}


	fn handle_irq(&self) -> bool
	{
		// SAFE: Readonly register
		let root_is = unsafe { self.inner.io_base.read_32(hw::REG_IS) };
		log_debug!("AHCI interrupt: IS={:#08x}", root_is);

		let mut rv = false;
		for port in &self.ports
		{
			if root_is & (1 << port.index) != 0
			{
				port.handle_irq();
				rv = true;
			}
		}
		rv
	}
}
impl device_manager::DriverInstance for Controller
{

}

impl<'a> PortRegs<'a>
{
	pub fn new(io: &device_manager::IOBinding, port_idx: usize) -> PortRegs {
		PortRegs {
			idx: port_idx,
			io: io
			}
	}

	fn read(&self, ofs: usize) -> u32 {
		assert!(ofs < 0x80);
		assert!(ofs & 3 == 0);
		// SAFE: None of the Px registers have a read side-effect
		unsafe { self.io.read_32(hw::REG_Px + self.idx * 0x80 + ofs) }
	}
	unsafe fn write(&self, ofs: usize, val: u32) {
		assert!(ofs < 0x80);
		self.io.write_32(hw::REG_Px + self.idx * 0x80 + ofs, val)
	}
}

// Maximum number of commands before a single page can't be shared
const MAX_COMMANDS_FOR_SHARE: usize = (::kernel::PAGE_SIZE - 256) / (256 + 32);
const CMDS_PER_PAGE: usize = ::kernel::PAGE_SIZE / 0x100;


impl ::core::fmt::Display for Port
{
	fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result
	{
		write!(f, "AHCI ? Port {}", self.index)
	}
}
impl Port
{
	
	pub fn new(controller: ArefBorrow<ControllerInner>, idx: usize, max_commands: usize) -> Result<Port, device_manager::DriverBindError>
	{
		use kernel::PAGE_SIZE;
		use core::mem::size_of;
		log_trace!("Port::new(, idx={}, max_commands={})", idx, max_commands);

		assert!(idx < 32);
		assert!(max_commands <= 32);
		
		
		let cl_size = max_commands * size_of::<hw::CmdHeader>();

		// Command list
		// - Command list first (32 * max_commands)
		// - Up to MAX_COMMANDS_FOR_SHARE in 1024 -- 4096-256
		// - RcvdFis last
		let cl_page = try!( ::kernel::memory::virt::alloc_dma(64, 1, "AHCI") );

		// Allocate pages for the command table
		// TODO: Delay allocating memory until a device is detected on this port
		let cmdtab_pages = if max_commands <= MAX_COMMANDS_FOR_SHARE {
				// All fits in the CL page!
				
				// - Return empty allocations
				Default::default()
			}
			else {
				// Individual pages for the command table, but the RcvdFis and CL share
				let mut tab_pages: [AllocHandle; 4] = Default::default();
				let n_pages = (max_commands - MAX_COMMANDS_FOR_SHARE + CMDS_PER_PAGE-1) / CMDS_PER_PAGE;
				assert!(n_pages < 4);
				for i in 0 .. n_pages
				{
					tab_pages[i] = try!( ::kernel::memory::virt::alloc_dma(64, 1, "AHCI") );
				}
				tab_pages
			};


		// Initialise the command list and table entries
		{
			// SAFE: Doesn't alias, as we uniquely own cl_page
			let cl_ents = unsafe { cl_page.as_int_mut_slice(0, max_commands) };
			for (listent, tabent) in Iterator::zip( cl_ents.iter_mut(), (0 .. max_commands).map(|i| Self::cmdidx_to_ref(&cl_page, cl_size, &cmdtab_pages, i)) )
			{
				*listent = hw::CmdHeader::new( ::kernel::memory::virt::get_phys(tabent) );
				//*tabent = hw::CmdTable::new();
			}
		}

		// TODO: This function should be unsafe itself, to pass the buck of ensuring that it's not called twice
		// (questionable) SAFE: Exclusive access to this segment of registers
		unsafe {
			let regs = PortRegs::new(&controller.io_base, idx);
			let addr = ::kernel::memory::virt::get_phys( cl_page.as_ref::<()>(0) );
			regs.write(hw::REG_PxCLB , (addr >>  0) as u32);
			regs.write(hw::REG_PxCLBU, (addr >> 32) as u32);
			let addr = ::kernel::memory::virt::get_phys( cl_page.as_ref::<hw::RcvdFis>( ::kernel::PAGE_SIZE - size_of::<hw::RcvdFis>() ) );
			regs.write(hw::REG_PxFB , (addr >>  0) as u32);
			regs.write(hw::REG_PxFBU, (addr >> 32) as u32);

			regs.write(hw::REG_PxSACT, 0);
			// Interrupts on
			regs.write(hw::REG_PxSERR, 0x3FF783);
			regs.write(hw::REG_PxIS, !0);
			regs.write(hw::REG_PxIE, hw::PxIS_CPDS|hw::PxIS_DSS|hw::PxIS_PSS|hw::PxIS_DHRS|hw::PxIS_TFES);
			// Start command engine (Start, FIS Rx Enable)
			let cmd = regs.read(hw::REG_PxCMD);
			regs.write(hw::REG_PxCMD, cmd|hw::PxCMD_ST|hw::PxCMD_FRE);
		}


		Ok(Port {
			ctrlr: controller,
			index: idx,
			command_list_alloc: cl_page,
			command_tables: cmdtab_pages,

			command_events: (0 .. max_commands).map(|_| ::kernel::sync::EventChannel::new()).collect(),
			used_commands_sem: ::kernel::sync::Semaphore::new(max_commands as isize, max_commands as isize),
			used_commands: AtomicU32::new(0),

			issued_commands_bs: 0,
			})
	}

	pub fn handle_irq(&self)
	{
		let regs = self.regs();

		let int_status = regs.read(hw::REG_PxIS);
		log_trace!("{} - int_status={:#x}", self, int_status);

		// Cold Port Detection Status
		if int_status & hw::PxIS_CPDS != 0
		{
			log_notice!("{} - Presence change", self);
		}


		// "Task File Error Status"
		if int_status & hw::PxIS_TFES != 0
		{
			let tfd = regs.read(hw::REG_PxTFD);
			log_warning!("{} - Device pushed error: TFD={:#x}", self, tfd);
			// TODO: This should terminate all outstanding transactions with an error.
		}

		// Device->Host Register Update
		if int_status & hw::PxIS_DHRS != 0
		{
			log_notice!("{} - Device register update, RFIS={:?}", self, self.get_rcvd_fis().RFIS);
		}
		// PIO Setup FIS Update
		if int_status & hw::PxIS_PSS != 0
		{
			log_notice!("{} - PIO setup status update, PSFIS={:?}", self, self.get_rcvd_fis().PSFIS);
		}

		// Check commands
		//if int_status & hw::PxIS_DPS != 0
		//{
		let issued_commands = regs.read(hw::REG_PxCI);
		let active_commands = regs.read(hw::REG_PxSACT);
		let error_commands = regs.read(hw::REG_PxSERR);
		let used_commands = self.used_commands.load(Ordering::Relaxed);
		log_trace!("used_commands = {:#x}, issued_commands={:#x}, active_commands={:#x}, error_commands={:#x}",
			used_commands, issued_commands, active_commands, error_commands);
		for cmd in 0 .. self.ctrlr.max_commands as usize
		{
			let mask = 1 << cmd;
			if used_commands & mask != 0
			{
				if issued_commands & mask == 0 || active_commands & mask == 0 {
					self.command_events[cmd].post();
				}
				else if error_commands & mask != 0 {
					log_notice!("{} - Command {} errored", self, cmd);
					self.command_events[cmd].post();
				}
				else {
					// Not yet complete
				}
			}
			else if active_commands & mask != 0	{
				log_warning!("{} - Command {} active, but not used", self, cmd);
			}
			else {
			}
		}
		//}
	
		// SAFE: ACK interrupt now that we're done
		unsafe {
			regs.write(hw::REG_PxIS, int_status);
		}
	}

	fn get_rcvd_fis(&self) -> &hw::RcvdFis
	{
		self.command_list_alloc.as_ref::<hw::RcvdFis>( ::kernel::PAGE_SIZE - ::core::mem::size_of::<hw::RcvdFis>() )
	}

	fn cmdidx_to_ref<'a>(cl_page: &'a AllocHandle, cl_size: usize, cmdtab_pages: &'a [AllocHandle], i: usize) -> &'a hw::CmdTable {
		let n_shared = (::kernel::PAGE_SIZE - cl_size) / 0x100 - 1;
		if i < n_shared {
			&cl_page.as_slice(cl_size, n_shared)[i]
		}
		else {
			let i = i - n_shared;
			let (pg,ofs) = (i / CMDS_PER_PAGE, i % CMDS_PER_PAGE);
			&cmdtab_pages[pg].as_slice(0, CMDS_PER_PAGE)[ofs]
		}
	}
	fn get_cmdtab_ptr(&self, idx: usize) -> *mut hw::CmdTable
	{
		// TODO: Does the fact that this returns &-ptr break anything?
		let r = Self::cmdidx_to_ref(&self.command_list_alloc, self.ctrlr.max_commands as usize * ::core::mem::size_of::<hw::CmdHeader>(), &self.command_tables,  idx);
		r as *const _ as *mut _
	}

	fn regs(&self) -> PortRegs {
		PortRegs {
			idx: self.index,
			io: &self.ctrlr.io_base,
			}
	}

	// Re-check the port for a new device
	pub fn update_connection(&self)
	{
		let io = self.regs();

		// SAFE: Status only registers
		let (tfd, ssts) = (io.read(hw::REG_PxTFD), io.read(hw::REG_PxSSTS));

		if tfd & (hw::PxTFD_STS_BSY|hw::PxTFD_STS_DRQ) != 0 {
			return ;
		}
		// SATA Status: Detected. 3 = Connected and PHY up
		if (ssts & hw::PxSSTS_DET) >> hw::PxSSTS_DET_ofs != 3 {
			return ;
		}
		

		// SAFE: Read has no side-effect
		match io.read(hw::REG_PxSIG)
		{
		0x00000101 => {
			// Standard ATA
			// Request ATA Identify from the disk
			const ATA_IDENTIFY: u8 = 0xEC;
			let ident = self.request_identify(ATA_IDENTIFY).expect("Failure requesting ATA identify");

			log_debug!("ATA `IDENTIFY` response data = {:?}", ident);
			
			let sectors = if ident.sector_count_48 == 0 { ident.sector_count_28 as u64 } else { ident.sector_count_48 };
			log_log!("{}: Hard Disk, {} sectors, {}", self, sectors, storage::SizePrinter(sectors * 512));
			// TODO: Create a volume descriptor pointing back to this disk/port
			},
		0xEB140101 => {
			// ATAPI Device
			const ATA_IDENTIFY_PACKET: u8 = 0xA1;
			let ident = self.request_identify(ATA_IDENTIFY_PACKET).expect("Failure requesting ATA IDENTIFY PACKET");
			log_warning!("TODO: ATAPI on {}, ident={:?}", self, ident);
			},
		signature @ _ => {
			log_error!("{} - Unknown signature {:08x}", self, signature);
			},
		}
	}

	fn request_identify(&self, cmd: u8) -> Result<::storage_ata::AtaIdentifyData, u16>
	{
		let mut ata_identify_data = ::storage_ata::AtaIdentifyData::default();
		try!( self.request_ata_lba28(0, cmd, 0,0, DataPtr::Recv(::kernel::lib::as_byte_slice_mut(&mut ata_identify_data))) );

		fn flip_bytes(bytes: &mut [u8]) {
			for pair in bytes.chunks_mut(2) {
				pair.swap(0, 1);
			}
		}
		// All strings are sent 16-bit endian flipped, so reverse that
		flip_bytes(&mut ata_identify_data.serial_number);
		flip_bytes(&mut ata_identify_data.firmware_ver);
		flip_bytes(&mut ata_identify_data.model_number);
		Ok( ata_identify_data )
	}

	fn request_ata_lba28(&self, disk: u8, cmd: u8,  n_sectors: u8, lba: u32, data: DataPtr) -> Result<usize, u16>
	{
		assert!(lba < (1<<24));
		let cmd_data = hw::sata::FisHost2DevReg {
			ty: hw::sata::FisType::H2DRegister as u8,
			flags: 0x80,
			command: cmd,
			sector_num: lba as u8,
			cyl_low: (lba >> 8) as u8,
			cyl_high: (lba >> 16) as u8,
			dev_head: 0x40 | (disk << 4) | (lba >> 24) as u8,
			sector_num_exp: 0,
			sector_count: n_sectors,
			sector_count_exp: 0,
			..Default::default()
			};
		self.do_fis(cmd_data.as_ref(), &[], data);
		Ok( 0 )
	}

	/// Create and dispatch a FIS
	fn do_fis(&self, cmd: &[u8], pkt: &[u8], data: DataPtr)
	{
		use kernel::memory::virt::get_phys;

		log_trace!("do_fis(cmd={:p}+{}, pkt={:p}+{}, data={:?})",
			cmd.as_ptr(), cmd.len(), pkt.as_ptr(), pkt.len(), data);

		let mut slot = self.get_command_slot();

		slot.data.cmd_fis.clone_from_slice(cmd);
		slot.data.atapi_cmd.clone_from_slice(pkt);

		// Generate the scatter-gather list
		let mut va = data.as_slice().as_ptr() as usize;
		let mut len = data.as_slice().len();
		let mut n_prdt_ents = 0;
		while len > 0
		{
			let base_phys = get_phys(va as *const u8);
			let mut seglen = ::kernel::PAGE_SIZE - base_phys as usize % ::kernel::PAGE_SIZE;
			const MAX_SEG_LEN: usize = (1 << 22);
			// Each entry must be contigious, and not >4MB
			while seglen < len && seglen <= MAX_SEG_LEN && get_phys( (va + seglen-1) as *const u8 ) == base_phys + (seglen-1) as u64
			{
				seglen += ::kernel::PAGE_SIZE;
			}
			let seglen = ::core::cmp::min(len, seglen);
			let seglen = ::core::cmp::min(MAX_SEG_LEN, seglen);
			if base_phys % 4 != 0 || seglen % 2 != 0 {
				todo!("AHCI Port::do_fis - Use a bounce buffer if alignment requirements are not met");
			}
			slot.data.prdt[n_prdt_ents].DBA = base_phys as u64;
			slot.data.prdt[n_prdt_ents].DBC = (seglen - 1) as u32;

			va += seglen;
			len -= seglen;

			n_prdt_ents += 1;
		}
		slot.data.prdt[n_prdt_ents-1].DBC |= 1 << 31;	// set IOC
		slot.hdr.PRDTL = n_prdt_ents as u16;
		slot.hdr.Flags = (if data.is_send() { 1 << 6 } else { 0 }) | (cmd.len() / 4) as u16;

		slot.event.clear();
		// SAFE: Wait ensures that memory stays valid
		unsafe {
			slot.start();
			slot.wait();
		}
	}

	fn get_command_slot(&self) -> CommandSlot
	{
		let max_commands = self.ctrlr.max_commands as usize;

		// 0. Request slot from semaphore
		self.used_commands_sem.acquire();
		
		// 1. Load
		let mut cur_used_commands = self.used_commands.load(Ordering::Relaxed);
		loop
		{
			// 2. Search
			let mut avail = self.ctrlr.max_commands as usize;
			for i in 0 .. self.ctrlr.max_commands as usize
			{
				if cur_used_commands & 1 << i == 0 {
					avail = i;
					break ;
				}
			}
			assert!(avail < self.ctrlr.max_commands as usize);

			// 3. Try and commit
			let try_new_val = cur_used_commands | (1 << avail);
			let newval = self.used_commands.compare_and_swap(cur_used_commands, try_new_val, Ordering::Acquire);

			if newval == cur_used_commands
			{
				// If successful, return
				// SAFE: Exclusive access
				let (tab, hdr) = unsafe {
					(
						&mut *self.get_cmdtab_ptr(avail),
						&mut self.command_list_alloc.as_int_mut_slice(0, max_commands)[avail],
						)
					};
				return CommandSlot {
					idx: avail as u8,
					port: self,
					data: tab,
					hdr: hdr,
					event: &self.command_events[avail],
					};
			}

			cur_used_commands = newval;
		}
	}
}

struct CommandSlot<'a> {
	idx: u8,
	port: &'a Port,
	pub data: &'a mut hw::CmdTable,
	pub hdr: &'a mut hw::CmdHeader,
	pub event: &'a EventChannel,
}
impl<'a> CommandSlot<'a>
{
	// UNSAFE: Caller must ensure that memory pointed to by the `data` table stays valid until the command is complete
	pub unsafe fn start(&self)
	{
		let mask = 1 << self.idx as usize;
		self.port.regs().write(hw::REG_PxSACT, mask);
		self.port.regs().write(hw::REG_PxCI, mask);
	}

	pub fn wait(&self)
	{
		self.event.sleep();

		let regs = self.port.regs();
		let (active, error) = (regs.read(hw::REG_PxCI), regs.read(hw::REG_PxSERR));

		let mask = 1 << self.idx;
		if active & mask == 0 {
			// All good
		}
		else if error & mask == 0 {
			// Still running?
			panic!("{} - Command {} woken while still active", self.port, self.idx);
		}
		else {
			panic!("{} - Command {} errored", self.port, self.idx);
		}
	}
}

impl<'a> ::core::ops::Drop for CommandSlot<'a>
{
	fn drop(&mut self)
	{
		let mask = 1 << self.idx;
		let regs = self.port.regs();
		// SAFE: Reading has no effect
		let cur_active = regs.read(hw::REG_PxCI) /* | regs.read(hw::REG_PxSACT) */;
		if cur_active & mask != 0 {
			todo!("CommandSlot::drop - Port {} cmd {} - Still active", self.port.index, self.idx);
		}
		
		// Release into the pool
		loop
		{
			let cur = self.port.used_commands.load(Ordering::Relaxed);
			let new = self.port.used_commands.compare_and_swap(cur, cur & !mask, Ordering::Release);
			if new == cur {
				break ;
			}
		}
		self.port.used_commands_sem.release();
	}
}

