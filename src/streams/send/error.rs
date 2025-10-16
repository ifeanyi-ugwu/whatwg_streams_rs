use std::{error::Error, fmt, sync::Arc};

#[derive(Debug, Clone)]
pub enum StreamError {
    Canceled,
    Aborted(Option<String>),
    Closing,
    Closed,
    TaskDropped,
    Other(Arc<dyn Error + Send + Sync>),
}

impl StreamError {
    /// Wrap any error type into `StreamError`
    pub fn other<E>(e: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        StreamError::Other(Arc::new(e))
    }

    /// Wrap a boxed error
    pub fn other_boxed(e: Box<dyn Error + Send + Sync>) -> Self {
        StreamError::Other(e.into())
    }
}

impl From<&str> for StreamError {
    fn from(s: &str) -> Self {
        #[derive(Debug)]
        struct SimpleError(String);
        impl fmt::Display for SimpleError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl Error for SimpleError {}
        StreamError::Other(Arc::new(SimpleError(s.to_string())))
    }
}

impl From<String> for StreamError {
    fn from(s: String) -> Self {
        StreamError::from(s.as_str())
    }
}

impl From<std::io::Error> for StreamError {
    fn from(e: std::io::Error) -> Self {
        StreamError::Other(Arc::new(e))
    }
}

impl From<Box<dyn Error + Send + Sync>> for StreamError {
    fn from(e: Box<dyn Error + Send + Sync>) -> Self {
        StreamError::Other(e.into())
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
            impl From<$error_type> for $crate::streams::send::error::StreamError {
                fn from(e: $error_type) -> Self {
                    $crate::streams::send::error::StreamError::Other(std::sync::Arc::new(e))
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
            StreamError::TaskDropped => write!(f, "Stream task was dropped"),
            StreamError::Other(err) => write!(f, "{}", err),
        }
    }
}

impl Error for StreamError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            StreamError::Other(err) => Some(err.as_ref()),
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

        let _: StreamError = StreamError::other(CustomError);
    }

    #[test]
    fn test_question_mark_works() -> Result<(), Box<dyn Error>> {
        fn returns_stream_error() -> Result<(), StreamError> {
            Err("stream error".into())
        }

        returns_stream_error()?; // `?` works
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
            might_fail()?; // works via macro
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
        might_fail_json()?; // works via From<Box<dyn Error>>

        Ok(())
    }
}
