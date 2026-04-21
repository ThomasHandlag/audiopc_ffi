use biquad::{Biquad, DirectForm1};

pub struct Effects {
   pub comb: Option<DirectForm1<f32>>,
   pub peak: Option<DirectForm1<f32>>,
   pub low_shelf: Option<DirectForm1<f32>>,
   pub high_shelf: Option<DirectForm1<f32>>,
   pub band_pass: Option<DirectForm1<f32>>,
   pub high_pass: Option<DirectForm1<f32>>,
   pub notch: Option<DirectForm1<f32>>,
   pub low_pass: Option<DirectForm1<f32>>,
}

impl Effects {
    pub fn new() -> Self {
        Self {
            comb: None,
            peak: None,
            low_shelf: None,
            high_shelf: None,
            band_pass: None,
            notch: None,
            high_pass: None,
            low_pass: None,
        }
    }

    pub fn process(&mut self, sample: f32) -> f32 {
        let mut s = sample;
        if let Some(comb) = &mut self.comb {
            s = comb.run(s);
        }
        if let Some(peak) = &mut self.peak {
            s = peak.run(s);
        }
        if let Some(low_shelf) = &mut self.low_shelf {
            s = low_shelf.run(s);
        }
        if let Some(high_shelf) = &mut self.high_shelf {
            s = high_shelf.run(s);
        }
        if let Some(band_pass) = &mut self.band_pass {
            s = band_pass.run(s);
        }
        if let Some(notch) = &mut self.notch {
            s = notch.run(s);
        }
        if let Some(low_pass) = &mut self.low_pass {
            s = low_pass.run(s);
        }
        if let Some(high_pass) = &mut self.high_pass {
            s = high_pass.run(s);
        }
        s
    }
}