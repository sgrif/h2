// ===== Build a codec from raw bytes =====

#[macro_export]
macro_rules! raw_codec {
    (
        $(
            $fn:ident => [$($chunk:expr,)+];
        )*
    ) => {{
        use futures::compat::*;
        let mut b = $crate::mock_io::Builder::new();

        $({
            let mut chunk = vec![];

            $(
                $crate::raw::Chunk::push(&$chunk, &mut chunk);
            )+

            b.$fn(&chunk[..]);
        })*

        $crate::Codec::new(b.build().compat()).compat()
    }}
}

pub trait Chunk {
    fn push(&self, dst: &mut Vec<u8>);
}

impl Chunk for u8 {
    fn push(&self, dst: &mut Vec<u8>) {
        dst.push(*self);
    }
}

impl<'a> Chunk for &'a [u8] {
    fn push(&self, dst: &mut Vec<u8>) {
        dst.extend(*self)
    }
}

impl<'a> Chunk for &'a str {
    fn push(&self, dst: &mut Vec<u8>) {
        dst.extend(self.as_bytes())
    }
}

impl Chunk for Vec<u8> {
    fn push(&self, dst: &mut Vec<u8>) {
        dst.extend(self.iter())
    }
}
