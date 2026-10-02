//! Decoder JPEG (baseline **dan** progressive) tanpa dependency tambahan.
//!
//! `--background` menerima PNG/BMP lewat [`crate::background`], dan JPEG
//! adalah format yang paling sering dipakai orang untuk foto. Tapi crate
//! `jpeg-encoder` yang sudah ada di `Cargo.toml` hanya bisa **encode**,
//! sedangkan crate `image` menarik ~10 dependency lain — unacceptable untuk
//! program yang biasanya jalan di ~12 MB RSS. Jadi decoder-nya ditulis di
//! sini.
//!
//! Cakupannya sengaja dibatasi ke yangopper realistis dipakai sebagai
//! background:
//!
//! - **Baseline sequential** (SOF0) dan **progressive** (SOF2).
//! - 1 komponen (grayscale) dan 3 komponen (YCbCr, atau RGB via Adobe APP14).
//! - Chroma subsampling 4:4:4 / 4:2:2 / 4:2:0 / 4:1:1 — di-upsample dengan
//!   replikasi.
//! - Restart marker (DRI) dan Huffman table yang tidak lengkap.
//!
//! Yang **tidak** didukung, dengan pesan jelas (bukan panic):
//!
//! - CMYK / YCCK / 4 komponen — perlu transform warna yang tidak sesederhana
//! - Arithmetic coding (SOF9/10/11/13..) — hampir tidak pernah dipakai.
//! - 12-bit sample depth.
//!
//! Catatan soal chroma subsampling: di-upsample dengan **replikasi**
//! (nearest neighbour), bukan interpolasi. Untuk background yang nanti
//! di-encode ulang ke JPEG dan dikirim ke LCD beresolusi 1920×462,selisihnya
//! tidak terlihat — dan interpolasi bilinear yang salah justru bisa
//!informatics gambar halos di tepi blok.
//!
//! Catatan soal IDCT: dipakai IDCT floating-point yang dipisah (row lalu
//! column) dengan tabel cosinus precomputed. IDCT integer AAN lebih cepat,
//! tapi akurasinya sedikit lebih rendah dan background sudah lewat dua
//! kali kompresi JPEG (file aslinya + encode ke LCD) — jadi benefit kecepatan
//! di sini tidak sebanding dengan unverifikasi yang lebih sulit.

/// Peta indeks zigzag → indeks natural dalam blok 8×8.
///
/// Ini urutan yang dipakai di bitstream JPEG: koefisien come *diurutkan
/// zigzag* (frekuensi tinggi di akhir), lalu diproses di urutan natural.
const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, //
    17, 24, 32, 25, 18, 11, 4, 5, //
    12, 19, 26, 33, 40, 48, 41, 34, //
    27, 20, 13, 6, 7, 14, 21, 28, //
    35, 42, 49, 56, 57, 50, 43, 36, //
    29, 22, 15, 23, 30, 37, 44, 51, //
    58, 59, 52, 45, 38, 31, 39, 46, //
    53, 60, 61, 54, 47, 55, 62, 63,
];

/// Panjang tabel cosinus IDCT.
///
/// Indeks yang dipakai adalah `(2x+1)*u` dengan `x, u ∈ 0..8`, jadi nilai
/// tertinginya `15 * 7 = 105`. Tabel harus lebih besar dari itu — tabel
/// berukuran 64 (natural untuk blok 8×8) akan panic di indeks terakhir.
const COS_LEN: usize = 128;

/// Tabel `cos(k * PI / 16)` untuk IDCT.
///
/// `cos` tidak bisa dipakai di `const fn`, jadi tabelnya dihitung sekali saat
/// pertama kali dipakai (lalu di-cache oleh `OnceLock`).
fn cos_table() -> &'static [f32; COS_LEN] {
    use std::sync::OnceLock;
    static T: OnceLock<[f32; COS_LEN]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0.0f32; COS_LEN];
        for (k, slot) in t.iter_mut().enumerate() {
            *slot = ((k as f32) * std::f32::consts::PI / 16.0).cos();
        }
        t
    })
}

/// Batas keras supaya file JPG|Jan yang ridiculously besar tidak membuat
/// alokasi GB. 60 juta piksel ~ 72 MB untuk RGB888, sudah jauh di atas
/// kebutuhan background 1920×462.
const MAX_PIXELS: u64 = 60_000_000;

/// Pesan error yang enak dibaca user (bukan debug-internal).
pub type Result<T> = std::result::Result<T, String>;

// ---------------------------------------------------------------------------
// Tabel Huffman

/// Tabel Huffman kanonik, bentuk "klasik" dari spesifikasi JPEG (Figure F.15).
///
/// Disimpan per panjang kode (`mincode`/`maxcode`/`valptr`) supaya decode
/// cukup beberapa perbandingan integer — jauh lebih cepat daripada
/// membangun pohon, dan implementasinya cuma belasan baris.
#[derive(Default, Clone)]
struct HuffTable {
    mincode: [i32; 17],
    maxcode: [i32; 17],
    valptr: [i32; 17],
    values: Vec<u8>,
}

impl HuffTable {
    fn build(counts: &[u8; 16], values: Vec<u8>) -> Result<Self> {
        // `maxcode[l] = -1` berarti "tidak ada kode sepanjang l".
        let mut mincode = [0i32; 17];
        let mut maxcode = [-1i32; 17];
        let mut valptr = [0i32; 17];

        let mut code: i32 = 0;
        let mut k: usize = 0;
        for l in 1..=16usize {
            let n = counts[l - 1] as usize;
            if n == 0 {
                maxcode[l] = -1;
            } else {
                if k + n > values.len() {
                    return Err(
                        "tabel Huffman JPEG: jumlah nilai kurang dari yang dideklarasikan".into(),
                    );
                }
                valptr[l] = k as i32;
                mincode[l] = code;
                code += n as i32;
                maxcode[l] = code - 1;
                k += n;
            }
            // Geser SELALU, bahkan kalau panjang ini kosong. Melewati
            // pergeseran di sini membuat semua kode بعد misalignment.
            code <<= 1;
        }

        Ok(Self { mincode, maxcode, valptr, values })
    }

    fn decode(&self, br: &mut BitReader) -> Result<u8> {
        let mut code = br.read_bit()? as i32;
        let mut l = 1usize;
        while l <= 16 {
            if self.maxcode[l] >= 0 && code <= self.maxcode[l] {
                let idx = (code - self.mincode[l] + self.valptr[l]) as usize;
                return self.values.get(idx).copied().ok_or_else(|| {
                    "nilai Huffman JPEG di luar jangkauan tabel (file rusak?)".to_string()
                });
            }
            code = (code << 1) | br.read_bit()? as i32;
            l += 1;
        }
        Err("kode Huffman JPEG tidak valid (file rusak?)".into())
    }
}

// ---------------------------------------------------------------------------
// Pembaca bit

/// Membaca bit dari entropy-coded segment.
///
/// Menangani byte stuffing (`FF 00` → satu byte `0xFF`) dan berhenti rapi
/// saat ketemu marker, mengisi sisa bit dengan nol supaya pemanggil tidak
/// reads di luar blok data.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    byte: u8,
    /// Sisa bit yang belum dibaca di `byte`.
    nbits: u32,
    /// True setelah ketemu marker — bit berikutnya dibaca sebagai nol.
    hit_marker: bool,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self { data, pos, byte: 0, nbits: 0, hit_marker: false }
    }

    fn read_bit(&mut self) -> Result<u8> {
        if self.nbits == 0 {
            let mut b = *self
                .data
                .get(self.pos)
                .ok_or("data JPEG terpotong di tengah entropy-coded segment")?;
            self.pos += 1;
            if b == 0xFF {
                match self.data.get(self.pos) {
                    // Byte stuffing: 0x00 sesudahnya berarti 0xFF itu data.
                    Some(0x00) => self.pos += 1,
                    // Marker asli: data entropy habis. Kembalikan bit nol.
                    _ => {
                        self.pos -= 1;
                        b = 0;
                        self.hit_marker = true;
                    }
                }
            }
            self.byte = b;
            self.nbits = 8;
        }
        self.nbits -= 1;
        Ok((self.byte >> self.nbits) & 1)
    }

    fn read_bits(&mut self, n: u32) -> Result<i32> {
        debug_assert!(n <= 16);
        let mut v = 0i32;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()? as i32;
        }
        Ok(v)
    }

    /// Byte dalam bitstream setelah pembulatan ke batas byte, lalu lompati
    /// semua marker sampai marker non-RST berikutnya (dipakai restart interval).
    fn restart(&mut self) -> Result<()> {
        self.nbits = 0;
        // Marker RSTn harus sudah ada tepat di sini; kalau tidak, kemungkinan
        // besar file-nya rusak — tapi di file nyata kadang satu saja hilang,
        // jadi jangan keras di sini.
        while let Some(&b) = self.data.get(self.pos) {
            if b != 0xFF {
                self.pos += 1;
                continue;
            }
            match self.data.get(self.pos + 1) {
                Some(&0x00) => {
                    self.pos += 2;
                    continue;
                }
                Some(&m) if (0xD0..=0xD7).contains(&m) => {
                    self.pos += 2;
                    return Ok(());
                }
                _ => return Ok(()),
            }
        }
        Err("data JPEG terpotong sebelum restart marker".into())
    }
}

/// Perpanjang koefisien sesuai aturan "receive and extend" JPEG.
///
/// `t` adalah jumlah bit untuk nilai (bukan panjang kode). Nilai 0 berarti
/// semua bit adalah 1 dan itu di-remap ke negatif.
fn extend(v: i32, t: u32) -> i32 {
    if t == 0 {
        return 0;
    }
    if v < (1 << (t - 1)) {
        v - (1 << t) + 1
    } else {
        v
    }
}

// ---------------------------------------------------------------------------
// Komponen & scan

#[derive(Clone)]
struct Component {
    id: u8,
    /// Faktor sampling horizontal (1..=4).
    h: usize,
    /// Faktor sampling vertikal (1..=4).
    v: usize,
    quant_idx: usize,
    dc_tbl: usize,
    ac_tbl: usize,
    /// Lebar blok komponen = `ceil(plane_w / 8)`.
    blocks_w: usize,
    blocks_h: usize,
    /// Koefisien DCT mentah, `blocks_w * blocks_h * 64`.
    coeffs: Vec<i16>,
    /// Prediktor DC, di-reset tiap restart interval.
    pred: i32,
}

struct Scan {
    /// Indeks komponen di `components`.
    comps: Vec<usize>,
    /// Batas bawah/atas Indeks koefisien yang diproses (inklusif).
    ss: u8,
    se: u8,
    /// Successive approximation high/low bit.
    ah: u8,
    al: u8,
}

#[derive(Debug)]
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    /// `width * height * 3` byte, RGB888.
    pub pixels: Vec<u8>,
}

/// Decoder JPEG: state internal.
pub fn decode(bytes: &[u8]) -> Result<Decoded> {
    let mut d = Decoder::new(bytes);
    d.run()
}

struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    width: u32,
    height: u32,
    progressive: bool,
    max_h: usize,
    max_v: usize,
    mcus_x: usize,
    mcus_y: usize,
    restart_interval: usize,
    components: Vec<Component>,
    quant: [[u16; 64]; 4],
    dc_tables: Vec<HuffTable>,
    ac_tables: Vec<HuffTable>,
    /// Adobe APP14 colour transform: 0 = RGB/YCC, 1 = YCbCr, 2 = YCCK.
    adobe_transform: Option<u8>,
    seen_sof: bool,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            width: 0,
            height: 0,
            progressive: false,
            max_h: 1,
            max_v: 1,
            mcus_x: 0,
            mcus_y: 0,
            restart_interval: 0,
            components: Vec::new(),
            quant: [[0; 64]; 4],
            dc_tables: Vec::new(),
            ac_tables: Vec::new(),
            adobe_transform: None,
            seen_sof: false,
        }
    }

    fn u8_at(&self, off: usize) -> Result<u8> {
        self.data
            .get(off)
            .copied()
            .ok_or_else(|| "file JPEG terpotong".to_string())
    }

    fn u16_at(&self, off: usize) -> Result<u16> {
        Ok(u16::from_be_bytes([self.u8_at(off)?, self.u8_at(off + 1)?]))
    }

    fn run(&mut self) -> Result<Decoded> {
        if !self.data.starts_with(&[0xFF, 0xD8]) {
            return Err("bukan file JPEG (tanda tangan SOI hilang)".into());
        }
        self.pos = 2;

        while self.pos < self.data.len() {
            // Marker selalu diawali 0xFF, lalu byte 0xFF berulang diizinkan.
            let mut p = self.pos;
            while self.data.get(p) == Some(&0xFF) {
                p += 1;
            }
            if p >= self.data.len() {
                break;
            }
            let marker = self.data[p];
            self.pos = p + 1;
            match marker {
                // EOI: selesai.
                0xD9 => break,
                // Start of scan: punya payload, tapi bentuknya beda dari segment
                // biasa (header-nya lebih pendek), jadi ditangani terpisah.
                0xDA => self.read_scan()?,
                // Standalone marker tanpa payload: TEM, RST0-7, SOI.
                0x01 | 0xD0..=0xD7 | 0xD8 => continue,
                _ => {
                    let len = self.u16_at(self.pos)? as usize;
                    if len < 2 {
                        return Err("segment JPEG punya panjang tidak valid".into());
                    }
                    let seg_start = self.pos + 2;
                    let seg_end = self.pos + len;
                    if seg_end > self.data.len() {
                        return Err("segment JPEG melewati akhir file".into());
                    }
                    let seg = &self.data[seg_start..seg_end];
                    match marker {
                        0xC0 | 0xC1 => {
                            self.read_sof(seg, false)?;
                        }
                        0xC2 => {
                            self.read_sof(seg, true)?;
                        }
                        // SOF3 (lossless), SOF5/6/7 (differential), SOF9..15
                        // (arithmetic / lossless arithmetic) tidak didukung.
                        0xC3 | 0xC5..=0xC7 | 0xC9..=0xCF => {
                            let name = match marker {
                                0xC3 => "lossless (SOF3)",
                                0xC9..=0xCB => "arithmetic coding",
                                0xCD..=0xCF => "arithmetic coding lossless",
                                _ => "differential (SOF5-7)",
                            };
                            return Err(format!(
                                "JPEG {name} belum didukung — simpan ulang sebagai baseline atau progressive"
                            ));
                        }
                        0xC4 => self.read_dht(seg)?,
                        0xDB => self.read_dqt(seg)?,
                        0xDD => {
                            self.restart_interval = self.u16_at(seg_start)? as usize;
                        }
                        0xEE => {
                            // Adobe APP14: byte terakhir adalah transform.
                            if let Some(&t) = seg.last() {
                                self.adobe_transform = Some(t);
                            }
                        }
                        // APPn lain, COM, DNL: dilewati.
                        _ => {}
                    }
                    self.pos = seg_end;
                }
            }
        }

        if !self.seen_sof {
            return Err("tidak ada frame JPEG (SOF) di file ini".into());
        }
        self.reconstruct()
    }

    fn read_sof(&mut self, seg: &[u8], progressive: bool) -> Result<()> {
        if self.seen_sof {
            return Err("file JPEG punya lebih dari satu frame".into());
        }
        if seg.len() < 6 {
            return Err("segment SOF JPEG terpotong".into());
        }
        let precision = seg[0];
        if precision != 8 {
            return Err(format!(
                "JPEG {precision}-bit per sample belum didukung (hanya 8-bit)"
            ));
        }
        let h = u16::from_be_bytes([seg[1], seg[2]]) as u32;
        let w = u16::from_be_bytes([seg[3], seg[4]]) as u32;
        let ncomp = seg[5] as usize;

        if w == 0 || h == 0 {
            return Err("dimensi JPEG nol".into());
        }
        if w as u64 * h as u64 > MAX_PIXELS {
            return Err(format!(
                "gambar JPEG terlalu besar: {w}×{h} (batas {MAX_PIXELS} piksel)"
            ));
        }
        if ncomp != 1 && ncomp != 3 {
            return Err(format!(
                "JPEG {ncomp} komponen belum didukung (hanya grayscale atau YCbCr)"
            ));
        }
        if seg.len() < 6 + ncomp * 3 {
            return Err("daftar komponen di SOF terpotong".into());
        }

        self.width = w;
        self.height = h;
        self.progressive = progressive;

        self.components.clear();
        for i in 0..ncomp {
            let off = 6 + i * 3;
            let hs = (seg[off + 1] >> 4) as usize;
            let vs = (seg[off + 1] & 0x0F) as usize;
            if !(1..=4).contains(&hs) || !(1..=4).contains(&vs) {
                return Err(format!(
                    "faktor sampling JPEG {hs}×{vs} tidak valid"
                ));
            }
            self.components.push(Component {
                id: seg[off],
                h: hs,
                v: vs,
                quant_idx: (seg[off + 2] & 0x0F) as usize,
                dc_tbl: 0,
                ac_tbl: 0,
                blocks_w: 0,
                blocks_h: 0,
                coeffs: Vec::new(),
                pred: 0,
            });
        }

        self.max_h = self.components.iter().map(|c| c.h).max().unwrap_or(1);
        self.max_v = self.components.iter().map(|c| c.v).max().unwrap_or(1);
        // Dimensi dalam "unit MCU": semua komponen berbagi grid yang sama.
        self.mcus_x = (w as usize).div_ceil(8 * self.max_h).max(1);
        self.mcus_y = (h as usize).div_ceil(8 * self.max_v).max(1);

        for i in 0..ncomp {
            let (cw, ch) = self.plane_dims(i);
            let (bw, bh) = (cw.div_ceil(8), ch.div_ceil(8));
            let c = &mut self.components[i];
            c.blocks_w = bw;
            c.blocks_h = bh;
            c.coeffs = vec![0i16; bw * bh * 64];
            if c.quant_idx >= 4 {
                return Err(format!(
                    "indeks tabel kuantisasi JPEG {} di luar jangkauan",
                    c.quant_idx
                ));
            }
        }

        self.seen_sof = true;
        Ok(())
    }

    /// Dimensi (lebar, tinggi) plane untuk komponen ke-`idx`.
    fn plane_dims(&self, idx: usize) -> (usize, usize) {
        let c = &self.components[idx];
        let cw = ((self.width as usize) * c.h).div_ceil(self.max_h).max(1);
        let ch = ((self.height as usize) * c.v).div_ceil(self.max_v).max(1);
        (cw, ch)
    }

    fn read_dqt(&mut self, seg: &[u8]) -> Result<()> {
        let mut i = 0usize;
        while i < seg.len() {
            let pq_tq = seg[i];
            i += 1;
            let precision = pq_tq >> 4;
            let idx = (pq_tq & 0x0F) as usize;
            if idx >= 4 {
                return Err(format!("tabel kuantisasi JPEG {idx} di luar jangkauan"));
            }
            let step = if precision == 0 { 1 } else { 2 };
            for (k, nat) in ZIGZAG.iter().enumerate() {
                let off = i + k * step;
                let v: u16 = if precision == 0 {
                    self.seg_u8(seg, off)? as u16
                } else {
                    u16::from_be_bytes([self.seg_u8(seg, off)?, self.seg_u8(seg, off + 1)?])
                };
                if v == 0 {
                    return Err("koefisien kuantisasi JPEG nol".into());
                }
                self.quant[idx][*nat as usize] = v;
            }
            i += 64 * step;
        }
        Ok(())
    }

    fn seg_u8(&self, seg: &[u8], off: usize) -> Result<u8> {
        seg.get(off).copied().ok_or_else(|| "segment DQT JPEG terpotong".to_string())
    }

    fn read_dht(&mut self, seg: &[u8]) -> Result<()> {
        let mut i = 0usize;
        while i < seg.len() {
            let tc_th = seg[i];
            i += 1;
            let class = tc_th >> 4;
            let idx = (tc_th & 0x0F) as usize;
            if idx >= 4 {
                return Err(format!("tabel Huffman JPEG {idx} di luar jangkauan"));
            }
            if i + 16 > seg.len() {
                return Err("segment DHT JPEG terpotong".into());
            }
            let mut counts = [0u8; 16];
            counts.copy_from_slice(&seg[i..i + 16]);
            i += 16;
            let total: usize = counts.iter().map(|&c| c as usize).sum();
            if i + total > seg.len() {
                return Err("nilai Huffman JPEG terpotong".into());
            }
            let values = seg[i..i + total].to_vec();
            i += total;
            let table = HuffTable::build(&counts, values)?;
            if class == 0 {
                if idx >= self.dc_tables.len() {
                    self.dc_tables.resize(idx + 1, HuffTable::default());
                }
                self.dc_tables[idx] = table;
            } else {
                if idx >= self.ac_tables.len() {
                    self.ac_tables.resize(idx + 1, HuffTable::default());
                }
                self.ac_tables[idx] = table;
            }
        }
        Ok(())
    }

    fn read_scan(&mut self) -> Result<()> {
        let len = self.u16_at(self.pos)? as usize;
        if len < 2 {
            return Err("header scan JPEG tidak valid".into());
        }
        let ns = self.u8_at(self.pos + 2)? as usize;
        if self.pos + 3 + ns * 2 + 3 > self.data.len() {
            return Err("header scan JPEG terpotong".into());
        }
        let mut comps = Vec::with_capacity(ns);
        for i in 0..ns {
            let cs = self.u8_at(self.pos + 3 + i * 2)?;
            let tbls = self.u8_at(self.pos + 4 + i * 2)?;
            // Cari di daftar komponen FRAME, bukan di `comps` — `comps` baru saja
            // dibangun dan pada iterasi pertama masih kosong.
            let idx = self
                .components
                .iter()
                .position(|c| c.id == cs)
                .ok_or_else(|| format!("scan JPEG merujuk komponen {cs} yang tidak ada"))?;
            let sel = &mut self.components[idx];
            sel.dc_tbl = (tbls >> 4) as usize;
            sel.ac_tbl = (tbls & 0x0F) as usize;
            comps.push(idx);
        }
        let sp = self.pos + 3 + ns * 2;
        let scan = Scan {
            comps,
            ss: self.u8_at(sp)?,
            se: self.u8_at(sp + 1)?,
            ah: self.u8_at(sp + 2)? >> 4,
            al: self.u8_at(sp + 2)? & 0x0F,
        };
        if scan.ss > 63 || scan.se > 63 || scan.ss > scan.se {
            return Err(format!(
                "rentang koefisien scan JPEG {}-{} tidak valid",
                scan.ss, scan.se
            ));
        }
        if scan.ah != 0 && !self.progressive {
            return Err("successive approximation hanya valid di JPEG progressive".into());
        }
        if self.progressive && scan.ss != 0 && ns != 1 {
            return Err("scan AC progressive JPEG tidak boleh interleaved".into());
        }

        // Entropy data mulai setelah segment header.
        self.pos = sp + 3;
        self.decode_scan(&scan)?;

        // Lompati sisa entropy data sampai marker berikutnya yang bukan RST.
        while let Some(&b) = self.data.get(self.pos) {
            if b == 0xFF {
                match self.data.get(self.pos + 1) {
                    Some(0x00) | Some(0xFF) => {
                        self.pos += 1;
                        continue;
                    }
                    Some(&m) if (0xD0..=0xD7).contains(&m) => {
                        self.pos += 2;
                        continue;
                    }
                    _ => return Ok(()),
                }
            }
            self.pos += 1;
        }
        Ok(())
    }

    fn decode_scan(&mut self, scan: &Scan) -> Result<()> {
        for c in scan.comps.iter() {
            self.components[*c].pred = 0;
        }
        let mut br = BitReader::new(self.data, self.pos);

        // Scan non-interleaved punya satu blok per iterasi; interleaved punya
        // satu MCU (yang bisa berisi beberapa blok per komponen).
        let single = scan.comps.len() == 1;
        let iterations = if single { 0 } else { self.mcus_x * self.mcus_y };

        let mut mcu = 0usize;
        let mut eobrun: u32 = 0;

        loop {
            let blocks: Vec<(usize, usize, usize)> = if single {
                // Dijalankan per blok dalam plane komponen ini.
                if mcu >= self.single_block_count(scan) {
                    break;
                }
                let c = &self.components[scan.comps[0]];
                let bx = mcu % c.blocks_w;
                let by = mcu / c.blocks_w;
                vec![(scan.comps[0], bx, by)]
            } else {
                if mcu >= iterations {
                    break;
                }
                let mx = mcu % self.mcus_x;
                let my = mcu / self.mcus_x;
                let mut v = Vec::new();
                for &ci in &scan.comps {
                    let (h, vs) = {
                        let c = &self.components[ci];
                        (c.h, c.v)
                    };
                    for by in 0..vs {
                        for bx in 0..h {
                            v.push((ci, mx * h + bx, my * vs + by));
                        }
                    }
                }
                v
            };

            for (ci, bx, by) in blocks {
                let (ss, se, ah, al) = (scan.ss, scan.se, scan.ah, scan.al);
                if self.progressive {
                    self.decode_block_progressive(ci, bx, by, ss, se, ah, al, &mut br, &mut eobrun)?;
                } else {
                    self.decode_block_baseline(ci, bx, by, &mut br)?;
                }
            }

            mcu += 1;
            if single && mcu >= self.single_block_count(scan) {
                break;
            }
            if self.restart_interval > 0
                && mcu.is_multiple_of(self.restart_interval)
                && mcu < iterations.max(1)
            {
                br.restart()?;
                for c in scan.comps.iter() {
                    self.components[*c].pred = 0;
                }
                eobrun = 0;
            }
        }

        self.pos = br.pos;
        Ok(())
    }

    fn single_block_count(&self, scan: &Scan) -> usize {
        let c = &self.components[scan.comps[0]];
        c.blocks_w * c.blocks_h
    }

    #[allow(clippy::too_many_arguments)]
    fn decode_block_baseline(
        &mut self,
        ci: usize,
        bx: usize,
        by: usize,
        br: &mut BitReader,
    ) -> Result<()> {
        let (dc_tbl, ac_tbl) = {
            let c = &self.components[ci];
            (c.dc_tbl, c.ac_tbl)
        };
        if dc_tbl >= self.dc_tables.len() || ac_tbl >= self.ac_tables.len() {
            return Err("scan JPEG merujuk tabel Huffman yang belum didefinisikan".into());
        }

        let t = self.dc_tables[dc_tbl].decode(br)? as u32;
        if t > 16 {
            return Err("panjang koefisien DC JPEG tidak valid".into());
        }
        let diff = extend(br.read_bits(t)?, t);
        let pred = self.components[ci].pred + diff;

        let (bw, ref mut coeffs) = {
            let c = &mut self.components[ci];
            c.pred = pred;
            (c.blocks_w, c.coeffs.as_mut_slice())
        };
        if bx >= bw || by * bw >= coeffs.len() / 64 {
            return Ok(());
        }
        let base = (by * bw + bx) * 64;
        coeffs[base] = pred as i16;

        let ac = &self.ac_tables[ac_tbl];
        let mut k = 1usize;
        while k < 64 {
            let rs = ac.decode(br)?;
            let r = (rs >> 4) as usize;
            let s = (rs & 0x0F) as u32;
            if s == 0 {
                if r != 15 {
                    break; // EOB
                }
                k += 16; // 16 koefisien nol berurutan
                continue;
            }
            k += r;
            if k > 63 {
                break;
            }
            let v = extend(br.read_bits(s)?, s);
            coeffs[base + ZIGZAG[k] as usize] = v as i16;
            k += 1;
        }
        Ok(())
    }

    /// Decode satu blok untuk JPEG progressive. Empat kasus sekaligus:
    /// DC first, DC refine, AC first, AC refine.
    #[allow(clippy::too_many_arguments)]
    fn decode_block_progressive(
        &mut self,
        ci: usize,
        bx: usize,
        by: usize,
        ss: u8,
        se: u8,
        ah: u8,
        al: u8,
        br: &mut BitReader,
        eobrun: &mut u32,
    ) -> Result<()> {
        let (bw, blocks_h) = {
            let c = &self.components[ci];
            (c.blocks_w, c.blocks_h)
        };
        if bx >= bw || by >= blocks_h {
            return Ok(());
        }
        let base = (by * bw + bx) * 64;

        let (dc_tbl, ac_tbl) = {
            let c = &self.components[ci];
            (c.dc_tbl, c.ac_tbl)
        };
        // Validasi tabel dilakukan LAZIM, bukan di muka: scan progressive
        // sering hanya menunjuk tabel yang memang dipakai. Scan DC-only
        // (Ss=0, Se=0) boleh menulis byte selector tabel AC yang belum
        // didefinisikan — itu legal, karena tabel itu tidak akan dibaca.
        // Memvalidasi keduanya sekaligus akan menolak file yang sah.
        let have_dc = |s: &Self| dc_tbl < s.dc_tables.len();
        let have_ac = |s: &Self| ac_tbl < s.ac_tables.len();

        if ss == 0 {
            if ah == 0 {
                // --- DC first ---
                if !have_dc(self) {
                    return Err("scan JPEG merujuk tabel Huffman DC yang belum didefinisikan".into());
                }
                let t = self.dc_tables[dc_tbl].decode(br)? as u32;
                if t > 16 {
                    return Err("panjang koefisien DC JPEG tidak valid".into());
                }
                let diff = extend(br.read_bits(t)?, t);
                let c = &mut self.components[ci];
                c.pred += diff;
                let v = c.pred << al;
                c.coeffs[base] = v as i16;
            } else {
                // --- DC refine: satu bit per blok, tanpa Huffman ---
                // libjpeg memakai `|=` polos di sini. Koefisien sudah digeser
                // kiri `al` di scan DC first, jadi bit-barunya masih kosong dan
                // OR menutupnya dengan benar — termasuk untuk nilai negatif
                // (dua's complement).
                let p1 = 1i16 << al;
                let bit = br.read_bit()?;
                if bit == 1 {
                    self.components[ci].coeffs[base] |= p1;
                }
            }
            return Ok(());
        }

        // --- AC (first / refine) ---
        if !have_ac(self) {
            return Err("scan JPEG merujuk tabel Huffman AC yang belum didefinisikan".into());
        }
        if ah == 0 {
            // AC first: ada EOB run.
            if *eobrun > 0 {
                *eobrun -= 1;
                return Ok(());
            }
            let ac = &self.ac_tables[ac_tbl];
            let mut k = ss as usize;
            while k <= se as usize {
                let rs = ac.decode(br)?;
                let r = (rs >> 4) as u32;
                let s = (rs & 0x0F) as u32;
                if s == 0 {
                    if r < 15 {
                        // EOB run: panjang (2^r) - 1, plus `r` bit tambahan.
                        let mut run = 1u32 << r;
                        if r > 0 {
                            run += br.read_bits(r)? as u32;
                        }
                        // Blok ini ikut terhitung, jadi dikurangi satu.
                        *eobrun = run - 1;
                        return Ok(());
                    }
                    k += 16;
                    continue;
                }
                k += r as usize;
                if k > se as usize {
                    break;
                }
                let v = extend(br.read_bits(s)?, s) << al;
                self.components[ci].coeffs[base + ZIGZAG[k] as usize] = v as i16;
                k += 1;
            }
            return Ok(());
        }

        // --- AC refine ---
        //
        // Mengikuti libjpeg `decode_mcu_AC_refine` (jdphuff.c) baris demi
        // baris. Dua aturan di sini yang kalau diubah sedikit akan menggeser
        // SELURUH bitstream sesudahnya:
        //
        // 1. Correction bit(position `k` yang sudah non-nol) **hanya dibaca**
        //    kalau koefisien itu non-nol. Koefisien yang nol TIDAK
        //   lenturkan bit — justru itulah yang melanchai koefisien non-nol
        //    di antaranya. Dan bit yang sudah dibaca tapi tidak.apply (karena
        //    bit posisi `al` sudah terisi) tetap ikut hydrogelakan arang
        //    stream-nya.
        // 2. ZRL (r=15) TIDAK langsung melompat 16 posisi. Ia masuk ke
        //    `do..while` yang sama: posisinya naik satu per satu sambil
        //    membaca correction bit, dan berhenti setelah melewati tepat
        //    `r` koefisien yang masih nol. Melompat langsung 16 tanpa
        //    membaca bit membuat seluruh stream berikutnya tidak sinkron.
        let p1 = 1i16 << al;
        let m1 = (-1i16) << al;
        let se = se as usize;
        let ss = ss as usize;

        let mut k = ss;
        if *eobrun == 0 {
            while k <= se {
                let rs = self.huff_ac(ac_tbl, br)?;
                let r = (rs >> 4) as i32;
                let s = (rs & 0x0F) as usize;

                // `s` = tanda koefisien baru (p1 / m1), atau 0 kalau yang
                // dibaca adalah ZRL/EOB.
                let mut new_sign: i16 = 0;
                if s != 0 {
                    let bit = br.read_bit()?;
                    new_sign = if bit == 1 { p1 } else { m1 };
                } else if r != 15 {
                    // EOB run: 2^r + `r` bit tambahan. Blok ini ikut terhitung,
                    // jadi dikurangi satu di sini (bukan nanti).
                    // Penurunan satu terjadi di blok `if *eobrun > 0` di bawah
                    // (blok ini masih memakai correction bit untuk sisa
                    // koefisiennya). Kalau dikurangi di sini juga, EOB run
                    // jadi satu blok lebih pendek dari seharusnya.
                    let mut run = 1u32 << r;
                    if r > 0 {
                        run += br.read_bits(r as u32)? as u32;
                    }
                    *eobrun = run;
                    break;
                }

                // `do..while` sama dengan libjpeg: satu iterasi selalu jalan,
                // lalu lanjut selama k masih di dalam band. `r` dihitung
                // mundur hanya saat koefisien yang ditemukan masih nol.
                let mut rem = r;
                loop {
                    self.refine_one(ci, base, k, p1, m1, br)?;
                    let idx = base + ZIGZAG[k] as usize;
                    if self.components[ci].coeffs[idx] == 0 {
                        rem -= 1;
                        if rem < 0 {
                            break;
                        }
                    }
                    k += 1;
                    if k > se {
                        break;
                    }
                }
                if k > se {
                    break;
                }

                if new_sign != 0 {
                    let coeffs = &mut self.components[ci].coeffs;
                    coeffs[base + ZIGZAG[k] as usize] = new_sign;
                }
                k += 1;
            }
        }

        if *eobrun > 0 {
            // Sisa koefisien non-nol di blok ini masih memakai correction bit.
            // `k` sudah diset di atas: `ss` untuk blok yang tidak menyentuh
            // simbol Huffman, atau posisi setelah EOB.
            while k <= se {
                self.refine_one(ci, base, k, p1, m1, br)?;
                k += 1;
            }
            *eobrun -= 1;
        }
        Ok(())
    }

    /// Decode satu simbol dari tabel Huffman AC `idx`.
    fn huff_ac(&self, idx: usize, br: &mut BitReader) -> Result<u8> {
        self.ac_tables[idx].decode(br)
    }

    /// Terapkan satu correction bit ke koefisien pada posisi zigzag `k` milik
    /// komponen `ci`. Koefisien yang nol, atau yang bit posisi `al`-nya sudah
    /// terisi, tidak menyentuh apa pun (dan tidak membaca bit).
    fn refine_one(
        &mut self,
        ci: usize,
        base: usize,
        k: usize,
        p1: i16,
        m1: i16,
        br: &mut BitReader,
    ) -> Result<()> {
        let coeffs = &mut self.components[ci].coeffs;
        let idx = base + ZIGZAG[k] as usize;
        if coeffs[idx] != 0 {
            // Bit WAJIB dibaca walau posisi `al` sudah terisi — kalau dilewati,
            // seluruh bitstream sesudahnya bergeser. Yang dilakukan dengan bit
            // itu baru bisa dilewati: koefisien yang sudah punya bit di posisi
            // `al` memang tidak boleh naik lagi.
            let bit = br.read_bit()?;
            if bit == 1 && coeffs[idx] & p1 == 0 {
                coeffs[idx] = if coeffs[idx] >= 0 {
                    coeffs[idx] + p1
                } else {
                    coeffs[idx] + m1
                };
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Rekonstruksi

    fn reconstruct(&mut self) -> Result<Decoded> {
        let w = self.width as usize;
        let h = self.height as usize;

        // Decode tiap komponen ke plane grayscale-sized-nya.
        let mut planes: Vec<Vec<u8>> = Vec::with_capacity(self.components.len());
        let mut dims: Vec<(usize, usize)> = Vec::with_capacity(self.components.len());
        for i in 0..self.components.len() {
            let (pw, ph) = self.plane_dims(i);
            dims.push((pw, ph));
            planes.push(self.decode_plane(i, pw, ph)?);
        }

        let mut pixels = vec![0u8; w * h * 3];

        if self.components.len() == 1 {
            // Grayscale: plane yang sama disalin ke R, G, B.
            let p = &planes[0];
            for y in 0..h {
                for x in 0..w {
                    let v = p[y * w + x];
                    let o = (y * w + x) * 3;
                    pixels[o] = v;
                    pixels[o + 1] = v;
                    pixels[o + 2] = v;
                }
            }
            return Ok(Decoded { width: self.width, height: self.height, pixels });
        }

        // 3 komponen: YCbCr kecuali kalau Adobe bilang transform = 0 (RGB).
        let is_rgb = self.adobe_transform == Some(0);
        let y_max = self.max_h;
        let v_max = self.max_v;

        for y in 0..h {
            for x in 0..w {
                let o = (y * w + x) * 3;
                let mut s = [0i32; 3];
                for (ci, plane) in planes.iter().enumerate() {
                    let (pw, _) = dims[ci];
                    let c = &self.components[ci];
                    // Replikasi (nearest):.sample langsung dari plane kecil.
                    let sx = (x * c.h / y_max).min(pw - 1);
                    let sy = (y * c.v / v_max).min(plane.len() / pw - 1);
                    s[ci] = plane[sy * pw + sx] as i32;
                }

                if is_rgb {
                    pixels[o] = s[0] as u8;
                    pixels[o + 1] = s[1] as u8;
                    pixels[o + 2] = s[2] as u8;
                } else {
                    // YCbCr → RGB (ITU-R BT.601, rentang penuh).
                    let (yy, cb, cr) = (s[0] as f32, s[1] as f32 - 128.0, s[2] as f32 - 128.0);
                    pixels[o] = clamp_u8(yy + 1.402 * cr);
                    pixels[o + 1] = clamp_u8(yy - 0.344_136 * cb - 0.714_136 * cr);
                    pixels[o + 2] = clamp_u8(yy + 1.772 * cb);
                }
            }
        }

        Ok(Decoded { width: self.width, height: self.height, pixels })
    }

    /// IDCT + level shift untuk seluruh blok satu komponen.
    fn decode_plane(&self, ci: usize, pw: usize, ph: usize) -> Result<Vec<u8>> {
        let c = &self.components[ci];
        let qt = &self.quant[c.quant_idx];
        let mut plane = vec![0u8; pw * ph];
        let cos_t = cos_table();

        for by in 0..c.blocks_h {
            for bx in 0..c.blocks_w {
                let base = (by * c.blocks_w + bx) * 64;
                let mut out = [0u8; 64];
                idct(&c.coeffs[base..base + 64], qt, cos_t, &mut out);

                // Salin ke plane, hati-hati tepi yang tidak penuh 8×8.
                for y in 0..8 {
                    let py = by * 8 + y;
                    if py >= ph {
                        break;
                    }
                    for x in 0..8 {
                        let px = bx * 8 + x;
                        if px >= pw {
                            break;
                        }
                        plane[py * pw + px] = out[y * 8 + x];
                    }
                }
            }
        }
        Ok(plane)
    }
}

fn clamp_u8(v: f32) -> u8 {
    if v <= 0.0 {
        0
    } else if v >= 255.0 {
        255
    } else {
        v.round() as u8
    }
}

/// IDCT 8×8 dua dimensi, dipisah jadi transform 1-D baris lalu kolom.
fn idct(coeffs: &[i16], qt: &[u16; 64], cos_t: &[f32; COS_LEN], out: &mut [u8; 64]) {
    const INV_SQRT2: f32 = std::f32::consts::FRAC_1_SQRT_2;
    const HALF: f32 = 0.5;

    // Dequantize + baris.
    let mut tmp = [0f32; 64];
    for y in 0..8 {
        for x in 0..8 {
            let mut s = 0f32;
            for u in 0..8 {
                let coef = coeffs[y * 8 + u] as f32 * qt[y * 8 + u] as f32;
                if coef != 0.0 {
                    let cu = if u == 0 { INV_SQRT2 } else { 1.0 };
                    s += cu * coef * cos_t[(2 * x + 1) * u];
                }
            }
            tmp[y * 8 + x] = s * HALF;
        }
    }

    // Kolom + level shift.
    for x in 0..8 {
        for y in 0..8 {
            let mut s = 0f32;
            for v in 0..8 {
                let t = tmp[v * 8 + x];
                if t != 0.0 {
                    let cv = if v == 0 { INV_SQRT2 } else { 1.0 };
                    s += cv * t * cos_t[(2 * y + 1) * v];
                }
            }
            out[y * 8 + x] = clamp_u8(s * HALF + 128.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jpeg_encoder::{ColorType, Encoder};

    /// Encode piksel RGB jadi JPEG dengan quality & chroma subsampling tertentu,
    /// lalu decode balik.
    ///
    /// `sampling` = rasio subsampling: 1 = 4:4:4, 2 = 4:2:0. `jpeg_encoder`
    /// menulis **baseline** saja, jadi jalur progressive diuji terpisah lewat
    /// file contoh di `tests/`.
    fn round_trip(pixels: &[u8], w: u32, h: u32, quality: u8, sampling: u8) -> Decoded {
        let sf = match sampling {
            1 => jpeg_encoder::SamplingFactor::R_4_4_4,
            _ => jpeg_encoder::SamplingFactor::F_2_2,
        };
        let mut buf = Vec::new();
        let mut enc = Encoder::new(&mut buf, quality);
        enc.set_sampling_factor(sf);
        enc.encode(pixels, w as u16, h as u16, ColorType::Rgb).unwrap();
        decode(&buf).unwrap()
    }

    /// Buat gambar uji dengan struktur yang mudah dinilai: blok warna solid
    /// 8×8 (agar IDCT tidak perlusampul noise) + sedikit gradien.
    fn test_image(w: u32, h: u32) -> Vec<u8> {
        let mut px = vec![0u8; (w * h * 3) as usize];
        for y in 0..h {
            for x in 0..w {
                let o = ((y * w + x) * 3) as usize;
                let (r, g, b) = if (x / 8 + y / 8) % 2 == 0 {
                    (220u8, 30, 40)
                } else {
                    (20u8, 200, 90)
                };
                px[o] = r;
                px[o + 1] = g;
                px[o + 2] = b;
            }
        }
        px
    }

    #[test]
    fn decode_gambar_solid_hitam() {
        let px = vec![0u8; 32 * 32 * 3];
        let d = round_trip(&px, 32, 32, 90, 1);
        assert_eq!((d.width, d.height), (32, 32));
        // Semua piksel harus tetap hitam (tidak ada yang nyempit ke abu-abu).
        assert!(d.pixels.iter().all(|&v| v == 0), "hitam harus tetap hitam");
    }

    #[test]
    fn decode_gambar_solid_putih() {
        let px = vec![255u8; 16 * 16 * 3];
        let d = round_trip(&px, 16, 16, 90, 1);
        assert!(d.pixels.iter().all(|&v| v == 255), "putih harus tetap putih");
    }

    #[test]
    fn warna_merah_tetap_merah() {
        let mut px = vec![0u8; 16 * 16 * 3];
        for i in (0..px.len()).step_by(3) {
            px[i] = 255;
        }
        let d = round_trip(&px, 16, 16, 95, 1);
        let o = (8 * 16 + 8) * 3;
        let (r, g, b) = (d.pixels[o], d.pixels[o + 1], d.pixels[o + 2]);
        assert!(r > 200, "merah: R harus tinggi, dapat {r}");
        assert!(g < 60, "merah: G harus rendah, dapat {g}");
        assert!(b < 60, "merah: B harus rendah, dapat {b}");
    }

    #[test]
    fn dimensi_terkoreksi_benar() {
        // Ukuran bukan kelipatan 8 dan bukan kelipatan 16 — batas tepi yang
        // paling sering salah di IDCT.
        for (w, h) in [(1u32, 1u32), (7, 3), (17, 9), (23, 17), (100, 40)] {
            let px = test_image(w, h);
            let d = round_trip(&px, w, h, 90, 1);
            assert_eq!((d.width, d.height), (w, h), "dimensi {w}×{h} salah");
            assert_eq!(d.pixels.len(), (w * h * 3) as usize);
        }
    }

    #[test]
    fn blok_warna_kembali_dekat_aslinya() {
        // Piksel tengah tiap blok 8×8 harus mendekati warna aslinya —
        // artefak ringing paling besar di pinggir blok.
        let w = 32u32;
        let h = 32u32;
        let px = test_image(w, h);
        let d = round_trip(&px, w, h, 95, 1);

        for by in 0..4u32 {
            for bx in 0..4u32 {
                let x = bx * 8 + 4;
                let y = by * 8 + 4;
                let o = ((y * w + x) * 3) as usize;
                let (want_r, want_g, want_b): (i32, i32, i32) = if (bx + by) % 2 == 0 {
                    (220, 30, 40)
                } else {
                    (20, 200, 90)
                };
                let (r, g, b) = (d.pixels[o], d.pixels[o + 1], d.pixels[o + 2]);
                let diff = (i32::from(r) - want_r)
                    .abs()
                    .max((i32::from(g) - want_g).abs())
                    .max((i32::from(b) - want_b).abs());
                assert!(
                    diff <= 24,
                    "blok ({bx},{by}) jauh meleset: dapat ({r},{g},{b}), mau ({want_r},{want_g},{want_b}), selisih {diff}"
                );
            }
        }
    }

    #[test]
    fn reject_file_bukan_jpeg() {
        let err = decode(b"ini bukan jpeg").unwrap_err();
        assert!(err.contains("JPEG"), "pesan harus menyebut JPEG, dapat: {err}");
    }

    #[test]
    fn reject_soi_saja() {
        let err = decode(&[0xFF, 0xD8]).unwrap_err();
        assert!(err.contains("SOF") || err.contains("terpotong"), "dapat: {err}");
    }

    #[test]
    fn reject_marker_berubah_di_medio_stream() {
        let mut buf = Vec::new();
        Encoder::new(&mut buf, 90).encode(&test_image(16, 16), 16, 16, ColorType::Rgb).unwrap();
        // Putuskan di tengah -> entropy data terpotong.
        let truncated = &buf[..buf.len() * 2 / 3];
        match decode(truncated) {
            Ok(_) => {} // kebetulan masihKebetulan valid, tidak apa-apa
            Err(e) => assert!(!e.is_empty(), "pesan error tidak boleh kosong"),
        }
    }

    #[test]
    fn gambar_besar_ditolak_dengan_pesan_jelas() {
        // SOF palsu yang mengklaim dimensi gila -> harus ditolak, bukan
        // mencoba alokasi GB.
        let mut f = vec![0xFFu8, 0xD8];
        f.extend_from_slice(&[0xFF, 0xC0]);
        // Panjang segment: 2 (len) + 1 (precision) + 2 (h) + 2 (w) + 1 (ncomp)
        // + 3 per komponen.
        f.extend_from_slice(&(2u16 + 1 + 2 + 2 + 1 + 3).to_be_bytes());
        f.push(8); // precision
        // 8000×8000 = 64 juta piksel, melewati batas 60 juta. (4000×4000
        // hanya 16 juta — masih diterima, jadi angka di sini disengaja.)
        f.extend_from_slice(&8000u16.to_be_bytes());
        f.extend_from_slice(&8000u16.to_be_bytes());
        f.push(1); // 1 komponen
        f.push(1); // component id
        f.push(0x11); // sampling 1×1
        f.push(0); // tabel kuantisasi 0
        let err = decode(&f).unwrap_err();
        assert!(err.contains("terlalu besar"), "dapat: {err}");
    }

    #[test]
    fn empat_komponen_ditolak() {
        // SOF dengan 4 komponen (CMYK) harus ditolak dengan pesan jelas.
        let mut f = vec![0xFFu8, 0xD8];
        f.extend_from_slice(&[0xFF, 0xC0]);
        f.extend_from_slice(&(2u16 + 1 + 2 + 2 + 1 + 3 * 4).to_be_bytes());
        f.push(8);
        f.extend_from_slice(&16u16.to_be_bytes());
        f.extend_from_slice(&16u16.to_be_bytes());
        f.push(4);
        for i in 0..4 {
            f.push(i as u8 + 1);
            f.push(0x11);
            f.push(0);
        }
        let err = decode(&f).unwrap_err();
        assert!(err.contains("4 komponen"), "dapat: {err}");
    }
}