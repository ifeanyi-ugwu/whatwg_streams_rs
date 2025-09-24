use std::{error::Error, fmt, sync::Arc};

#[derive(Clone)]
pub struct ArcError(Arc<dyn Error + Send + Sync>);

impl fmt::Debug for ArcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for ArcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Error for ArcError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
}

impl From<&str> for ArcError {
    fn from(s: &str) -> Self {
        #[derive(Debug)]
        struct SimpleError(String);
        impl fmt::Display for SimpleError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl Error for SimpleError {}

        ArcError(Arc::new(SimpleError(s.to_string())))
    }
}

impl From<String> for ArcError {
    fn from(s: String) -> Self {
        ArcError::from(s.as_str())
    }
}

#[derive(Debug, Clone)]
pub enum StreamError {
    Canceled,
    Aborted(Option<String>),
    Closing,
    Closed,
    Custom(ArcError),
}

impl<E> From<E> for StreamError
where
    E: Error + Send + Sync + 'static,
{
    fn from(e: E) -> Self {
        StreamError::Custom(ArcError(Arc::new(e)))
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Canceled => write!(f, "Stream operation was canceled"),
            StreamError::Aborted(reason) => {
                if let Some(reason) = reason {
                    write!(f, "Stream was aborted: {}", reason)
                } else {
                    write!(f, "Stream was aborted")
                }
            }
            StreamError::Closing => write!(f, "Stream is closing"),
            StreamError::Closed => write!(f, "Stream is closed"),
            StreamError::Custom(err) => write!(f, "{}", err),
        }
    }
}
