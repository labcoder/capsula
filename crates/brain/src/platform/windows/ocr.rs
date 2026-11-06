//! `WinRT` English OCR; constructed and used only on the dedicated MTA thread.

use std::marker::PhantomData;
use std::rc::Rc;

use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::DataWriter;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::HSTRING;

use crate::screen::ocr::{Bitmap, Engine, Failure, NativeWord, Stop};

struct Apartment(PhantomData<Rc<()>>);
impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: this guard balances this thread's successful CoInitializeEx.
        unsafe { CoUninitialize() };
    }
}

pub(crate) struct Native {
    engine: OcrEngine,
    max: u32,
    _apartment: Apartment,
}
impl std::fmt::Debug for Native {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeOcr(..)")
    }
}

fn unavailable(_: windows::core::Error) -> Failure {
    Failure::Unavailable
}

pub(crate) fn open() -> Result<Native, Failure> {
    // SAFETY: only the dedicated OCR thread calls open; its apartment guard balances success.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map_err(unavailable)?;
    let apartment = Apartment(PhantomData);
    let language = Language::CreateLanguage(&HSTRING::from("en")).map_err(unavailable)?;
    let engine = OcrEngine::TryCreateFromLanguage(&language).map_err(unavailable)?;
    Ok(Native {
        engine,
        max: OcrEngine::MaxImageDimension().map_err(unavailable)?,
        _apartment: apartment,
    })
}

impl Engine for Native {
    fn max_dimension(&self) -> u32 {
        self.max
    }

    fn recognize(
        &mut self,
        image: &Bitmap,
        stop: &Stop,
        emit: &mut dyn FnMut(NativeWord) -> bool,
    ) -> Result<(), Failure> {
        stop.check()?;
        let writer = DataWriter::new().map_err(unavailable)?;
        writer
            .WriteBytes(image.rgba.as_raw())
            .map_err(unavailable)?;
        let buffer = writer.DetachBuffer().map_err(unavailable)?;
        let bitmap = SoftwareBitmap::CreateWithAlpha(
            BitmapPixelFormat::Rgba8,
            i32::try_from(image.rgba.width()).map_err(|_| Failure::Invalid)?,
            i32::try_from(image.rgba.height()).map_err(|_| Failure::Invalid)?,
            BitmapAlphaMode::Ignore,
        )
        .map_err(unavailable)?;
        bitmap.CopyFromBuffer(&buffer).map_err(unavailable)?;
        stop.check()?;
        let operation = self.engine.RecognizeAsync(&bitmap).map_err(unavailable)?;
        loop {
            if let Err(error) = stop.check() {
                operation.Cancel().map_err(unavailable)?;
                return Err(error);
            }
            if operation.Status().map_err(unavailable)?.0 != 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        stop.check()?;
        let result = operation.GetResults().map_err(unavailable)?;
        let lines = result.Lines().map_err(unavailable)?;
        for line in 0..lines.Size().map_err(unavailable)? {
            let words = lines
                .GetAt(line)
                .map_err(unavailable)?
                .Words()
                .map_err(unavailable)?;
            for index in 0..words.Size().map_err(unavailable)? {
                stop.check()?;
                let word = words.GetAt(index).map_err(unavailable)?;
                let rect = word.BoundingRect().map_err(unavailable)?;
                if !emit(NativeWord {
                    text: word.Text().map_err(unavailable)?.to_string(),
                    bounds: [rect.X, rect.Y, rect.Width, rect.Height].map(f64::from),
                }) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}
