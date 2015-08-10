
use geom::{Rect,Px};

#[derive(Copy,Clone)]
pub struct Colour(u32);
impl Colour
{
	pub fn black() -> Colour { Colour(0) }
	pub fn ltgray() -> Colour { Colour(0xDD_DD_DD) }
	pub fn gray() -> Colour { Colour(0x55_55_55) }
	pub fn white() -> Colour { Colour(0xFF_FF_FF) }
	pub fn as_argb32(&self) -> u32 { self.0 }
}

#[derive(Default)]
pub struct Surface
{
	width: usize,
	data: ::std::cell::RefCell<Vec<u32>>,
}

impl Surface
{
	fn height(&self) -> u32 {
		if self.width == 0 {
			assert_eq!(self.data.borrow().len(), 0);
			0
		}
		else {
			(self.data.borrow().len() / self.width) as u32
		}
	}
	pub fn blit_to_win(&self, win: &::syscalls::gui::Window) {
		win.blit_rect(0, 0, self.width as u32, self.height(), &self.data.borrow());
	}
	pub fn resize(&mut self, dims: ::syscalls::gui::Dims) {
		self.width = dims.w as usize;
		*self.data.borrow_mut() = vec![0x002200; (dims.w as usize * dims.h as usize)];
	}
	pub fn rect(&self) -> Rect<Px> {
		Rect::new(0, 0, self.width as u32, self.height())
	}
	pub fn slice(&self, rect: Rect<Px>) -> SurfaceView {
		let rect = self.rect().intersect(&rect);
		kernel_log!("Surface::slice - rect={:?}", rect);
		SurfaceView { surf: self, rect: rect }
	}

	fn foreach_scanlines<F: FnMut(usize, &mut [u32])>(&self, rect: Rect<Px>, mut f: F) {
		//kernel_log!("foreach_scanlines(rect={:?}, F={})", rect, type_name!(F));
		for (i, row) in self.data.borrow_mut().chunks_mut(self.width).skip(rect.y().0 as usize).take(rect.height().0 as usize).enumerate()
		{
			//kernel_log!("{}: {}  {}..{}", i, rect.y().0 as usize + i, rect.x().0, rect.x2().0);
			f( i, &mut row[rect.x().0 as usize .. rect.x2().0 as usize] );
		}
	}
}

pub struct SurfaceView<'a>
{
	surf: &'a Surface,
	rect: Rect<Px>,
}
impl<'a> SurfaceView<'a>
{
	pub fn width(&self) -> u32 { self.rect.width().0 }
	pub fn height(&self) -> u32 { self.rect.height().0 }
	pub fn slice(&self, rect: Rect<Px>) -> SurfaceView {
		SurfaceView {
			surf: self.surf,
			rect: self.rect.intersect(&rect.offset(self.rect.x(), self.rect.y())),
		}
	}

	fn foreach_scanlines<F: FnMut(usize, &mut [u32])>(&self, rect: Rect<Px>, f: F) {
		self.surf.foreach_scanlines( self.rect.relative(&rect), f )
	}

	pub fn fill_rect(&self, rect: Rect<Px>, colour: Colour) {
		self.foreach_scanlines(rect, |_, line|
			for px in line.iter_mut() {
				*px = colour.as_argb32();
			}
			);
	}

	pub fn draw_text<It: Iterator<Item=char>>(&self, mut rect: Rect<Px>, chars: It, colour: Colour) {
		let mut st = S_FONT.get_renderer();
		let mut chars = chars.peekable();
		kernel_log!("draw_text: rect = {:?}", rect);
		while let Some( (w,h) ) = st.render_grapheme(&mut chars, colour)
		{
			//kernel_log!("rect = {:?}", rect);
			self.foreach_scanlines(rect, |i, line| {
				for (d,s) in line.iter_mut().zip( st.buffer(i, w as usize) )
				{
					// TODO: Alpha blend
					match *s >> 24 {
					0 => { *d = *s; },
					255 => {},
					_ => panic!("TODO: Alpha blending"),
					}
					//*d = Colour::blend( Colour::from_argb32(*d), Colour::from_argb32(*s) );
					//*d = *s;
				}
				});
			rect = rect.offset(::geom::Px(w), ::geom::Px(0));
		}
	}
}

static S_FONT: MonoFont = MonoFont::new();
struct MonoFont;
impl MonoFont {
	const fn new() -> MonoFont { MonoFont }
	fn get_renderer(&self) -> MonoFontRender {
		MonoFontRender { buffer: [0; 8*16], }
	}
}

include!("../../Graphics/font_cp437_8x16.rs");

struct MonoFontRender {
	buffer: [u32; 8*16],
}
impl MonoFontRender
{
	pub fn render_grapheme<It: Iterator<Item=char>>(&mut self, it: &mut ::std::iter::Peekable<It>, colour: Colour) -> Option<(u32,u32)> {
		self.buffer = [0xFF_000000; 8*16];
		if let Some(ch) = it.next()
		{
			self.render_char(colour, ch);
			while it.peek().map(|c| c.is_combining()).unwrap_or(false)
			{
				self.render_char(colour, it.next().unwrap());
			}
			Some( (8,16) )
		}
		else {
			None
		}
	}
	pub fn buffer(&self, row: usize, width: usize) -> &[u32] {
		&self.buffer[row * 8..][..width]
	}

	/// Actually does the rendering
	fn render_char(&mut self, colour: Colour, cp: char)
	{
		let idx = unicode_to_cp437(cp);
		//kernel_log!("render_char - '{}' = {:#x}", cp, idx);
		
		let bitmap = &S_FONTDATA[idx as usize];
		
		// Actual render!
		for row in (0 .. 16)
		{
			let byte = &bitmap[row as usize];
			let base = row * 8;
			let r = &mut self.buffer[base .. base + 8]; 
			for col in (0usize .. 8)
			{
				if (byte >> 7-col) & 1 != 0 {
					r[col] = colour.as_argb32();
				}
			}
		}
	}
}

/// Trait to provde 'is_combining', used by render code
trait UnicodeCombining
{
	fn is_combining(&self) -> bool;
}

impl UnicodeCombining for char
{
	fn is_combining(&self) -> bool
	{
		match *self as u32
		{
		// Ranges from wikipedia:Combining_Character
		0x0300 ... 0x036F => true,
		0x1AB0 ... 0x1AFF => true,
		0x1DC0 ... 0x1DFF => true,
		0x20D0 ... 0x20FF => true,
		0xFE20 ... 0xFE2F => true,
		_ => false,
		}
	}
}