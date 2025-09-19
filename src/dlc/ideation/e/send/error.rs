use std::{error::Error, fmt, ops::Deref, sync::Arc};

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
pub struct OtherError(pub Arc<dyn Error + Send + Sync>);

impl fmt::Display for OtherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Error for OtherError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
}

impl Deref for OtherError {
    type Target = Arc<dyn Error + Send + Sync>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl OtherError {
    pub fn new<E>(e: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        OtherError(Arc::new(e))
    }
}

impl From<std::io::Error> for OtherError {
    fn from(e: std::io::Error) -> Self {
        OtherError(Arc::new(e))
    }
}

macro_rules! impl_from_error_for_other {
    ($($error_type:ty),* $(,)?) => {
        $(
            impl From<$error_type> for OtherError {
                fn from(e: $error_type) -> Self {
                    OtherError(Arc::new(e))
                }
            }
        )*
    };
}

#[derive(Debug, Clone)]
pub enum StreamError {
    Canceled,
    Aborted(Option<String>),
    Closing,
    Closed,
    Custom(ArcError),
    Other(OtherError),
}

impl From<ArcError> for StreamError {
    fn from(e: ArcError) -> Self {
        StreamError::Custom(e)
    }
}

impl From<String> for StreamError {
    fn from(s: String) -> Self {
        StreamError::Custom(ArcError::from(s))
    }
}

impl From<&str> for StreamError {
    fn from(s: &str) -> Self {
        StreamError::Custom(ArcError::from(s))
    }
}

impl From<std::io::Error> for StreamError {
    fn from(e: std::io::Error) -> Self {
        StreamError::Custom(ArcError(Arc::new(e)))
    }
}

impl From<OtherError> for StreamError {
    fn from(e: OtherError) -> Self {
        StreamError::Other(e)
    }
}

impl From<Box<dyn Error + Send + Sync>> for StreamError {
    fn from(e: Box<dyn Error + Send + Sync>) -> Self {
        StreamError::Other(OtherError(e.into()))
    }
}

impl StreamError {
    pub fn other<E: Error + Send + Sync + 'static>(e: E) -> Self {
        StreamError::Other(OtherError::new(e))
    }

    pub fn other_boxed(e: Box<dyn Error + Send + Sync>) -> Self {
        StreamError::Other(OtherError(e.into()))
    }
}

/// Macro for users to add direct `From` implementations for their error types.
/// This allows using `?` directly without `.map_err(StreamError::other)`.
///
/// # Example
/// ```rust
/// use your_crate::{StreamError, impl_stream_error_from};
///
/// impl_stream_error_from!(
///     serde_json::Error,
///     reqwest::Error,
///     your_custom::Error,
/// );
///
/// // Now you can use ? directly:
/// fn example() -> Result<(), StreamError> {
///     let data = serde_json::from_str("{}")?;  // Direct ? works!
///     Ok(())
/// }
/// ```
#[macro_export]
macro_rules! impl_stream_error_from {
    ($($error_type:ty),* $(,)?) => {
        $(
            impl From<$error_type> for $crate::dlc::ideation::e::send::error::StreamError {
                fn from(e: $error_type) -> Self {
                    $crate::dlc::ideation::e::send::error::StreamError::Other($crate::dlc::ideation::e::send::error::OtherError::new(e))
                }
            }
        )*
    };
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StreamError::Canceled => write!(f, "Stream operation was canceled"),
            StreamError::Aborted(Some(reason)) => write!(f, "Stream was aborted: {}", reason),
            StreamError::Aborted(None) => write!(f, "Stream was aborted"),
            StreamError::Closing => write!(f, "Stream is closing"),
            StreamError::Closed => write!(f, "Stream is closed"),
            StreamError::Custom(err) => write!(f, "{}", err),
            StreamError::Other(err) => write!(f, "{}", err),
        }
    }
}

impl Error for StreamError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            StreamError::Custom(err) => Some(&*err.0),
            StreamError::Other(err) => Some(&**err),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_conversions_work() {
        let _: StreamError = "error message".into();
        let _: StreamError = String::from("error").into();

        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "io error");
        let _: StreamError = io_err.into();

        #[derive(Debug)]
        struct CustomError;
        impl fmt::Display for CustomError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "custom error")
            }
        }
        impl Error for CustomError {}

        let other_err = OtherError::new(CustomError);
        let _: StreamError = other_err.into();
    }

    #[test]
    fn test_question_mark_works() -> Result<(), Box<dyn Error>> {
        fn returns_stream_error() -> Result<(), StreamError> {
            Err("stream error".into())
        }

        returns_stream_error()?;
        Ok(())
    }

    #[test]
    fn test_macro_usage_example() -> Result<(), Box<dyn Error>> {
        #[derive(Debug)]
        struct UserCustomError(String);
        impl fmt::Display for UserCustomError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "User error: {}", self.0)
            }
        }
        impl Error for UserCustomError {}

        impl_stream_error_from!(UserCustomError);

        fn user_function() -> Result<(), StreamError> {
            fn might_fail() -> Result<(), UserCustomError> {
                Err(UserCustomError("something went wrong".to_string()))
            }
            might_fail()?; // works directly
            Ok(())
        }

        assert!(user_function().is_err());
        Ok(())
    }

    #[test]
    fn test_mixed_error_handling() -> Result<(), StreamError> {
        fn might_fail_json() -> Result<(), Box<dyn Error + Send + Sync>> {
            Err("json parse error".into())
        }

        #[derive(Debug)]
        struct CustomError(String);
        impl fmt::Display for CustomError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl Error for CustomError {}

        fn might_fail_custom() -> Result<(), CustomError> {
            Err(CustomError("custom failure".to_string()))
        }

        might_fail_json().map_err(StreamError::other_boxed)?;
        might_fail_custom().map_err(StreamError::other)?;
        might_fail_json()?;

        Ok(())
    }
}
