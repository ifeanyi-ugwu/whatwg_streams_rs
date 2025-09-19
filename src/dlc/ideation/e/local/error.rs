use std::{error::Error, fmt, ops::Deref, rc::Rc};

#[derive(Clone)]
pub struct RcError(Rc<dyn Error>);

impl fmt::Debug for RcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for RcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl Error for RcError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
}

impl From<&str> for RcError {
    fn from(s: &str) -> Self {
        #[derive(Debug)]
        struct SimpleError(String);
        impl fmt::Display for SimpleError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl Error for SimpleError {}

        RcError(Rc::new(SimpleError(s.to_string())))
    }
}

impl From<String> for RcError {
    fn from(s: String) -> Self {
        RcError::from(s.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct OtherError(pub Rc<dyn Error>);

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
    type Target = Rc<dyn Error>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl OtherError {
    pub fn new<E>(e: E) -> Self
    where
        E: Error + 'static,
    {
        OtherError(Rc::new(e))
    }
}

impl From<std::io::Error> for OtherError {
    fn from(e: std::io::Error) -> Self {
        OtherError(Rc::new(e))
    }
}

macro_rules! impl_from_error_for_other {
    ($($error_type:ty),* $(,)?) => {
        $(
            impl From<$error_type> for OtherError {
                fn from(e: $error_type) -> Self {
                    OtherError(Rc::new(e))
                }
            }
        )*
    };
}

// usage: impl_from_error_for_other!(serde_json::Error, your_custom::Error);

#[derive(Debug, Clone)]
pub enum StreamError {
    Canceled,
    Aborted(Option<String>),
    Closing,
    Closed,
    Custom(RcError),
    Other(OtherError),
}

impl From<RcError> for StreamError {
    fn from(e: RcError) -> Self {
        StreamError::Custom(e)
    }
}

impl From<String> for StreamError {
    fn from(s: String) -> Self {
        StreamError::Custom(RcError::from(s))
    }
}

impl From<&str> for StreamError {
    fn from(s: &str) -> Self {
        StreamError::Custom(RcError::from(s))
    }
}

impl From<std::io::Error> for StreamError {
    fn from(e: std::io::Error) -> Self {
        StreamError::Custom(RcError(Rc::new(e)))
    }
}

impl From<OtherError> for StreamError {
    fn from(e: OtherError) -> Self {
        StreamError::Other(e)
    }
}

impl From<Box<dyn Error>> for StreamError {
    fn from(e: Box<dyn Error>) -> Self {
        StreamError::Other(OtherError(e.into()))
    }
}

impl StreamError {
    pub fn other<E: Error + 'static>(e: E) -> Self {
        StreamError::Other(OtherError::new(e))
    }

    pub fn other_boxed(e: Box<dyn Error>) -> Self {
        StreamError::Other(OtherError(e.into()))
    }
}

#[macro_export]
macro_rules! impl_local_stream_error_from {
    ($($error_type:ty),* $(,)?) => {
        $(
            impl From<$error_type> for $crate::dlc::ideation::e::local::error::StreamError {
                fn from(e: $error_type) -> Self {
                    $crate::dlc::ideation::e::local::error::StreamError::Other($crate::dlc::ideation::e::local::error::OtherError::new(e))
                }
            }
        )*
    };
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
            StreamError::Other(err) => write!(f, "{}", err),
        }
    }
}

impl std::error::Error for StreamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StreamError::Custom(err) => Some(err.0.as_ref()),
            StreamError::Other(err) => Some(err.0.as_ref()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_all_conversions_work() {
        // String conversions
        let _: StreamError = "error message".into();
        let _: StreamError = String::from("error").into();

        // IO error
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "io error");
        let _: StreamError = io_err.into();

        // Custom error -> OtherError
        #[derive(Debug)]
        struct CustomError;
        impl fmt::Display for CustomError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "custom error")
            }
        }
        impl Error for CustomError {}

        let custom_err = CustomError;
        let other_err = OtherError::new(custom_err);
        let _: StreamError = other_err.into();
    }

    #[test]
    fn test_question_mark_works() -> Result<(), Box<dyn Error>> {
        fn returns_stream_error() -> Result<(), StreamError> {
            Err("stream error".into())
        }

        let result = returns_stream_error();
        assert!(result.is_err());
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

        impl_local_stream_error_from!(UserCustomError);

        fn user_function() -> Result<(), StreamError> {
            fn might_fail() -> Result<(), UserCustomError> {
                Err(UserCustomError("something went wrong".to_string()))
            }

            might_fail()?; // works via macro
            Ok(())
        }

        let result = user_function();
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn test_mixed_error_handling() -> Result<(), StreamError> {
        fn might_fail_json() -> Result<(), Box<dyn Error>> {
            Err("json parse error".into())
        }

        fn might_fail_custom() -> Result<(), CustomError> {
            Err(CustomError("custom failure".to_string()))
        }

        #[derive(Debug)]
        struct CustomError(String);
        impl fmt::Display for CustomError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl Error for CustomError {}

        might_fail_json().map_err(StreamError::other_boxed)?;
        might_fail_custom().map_err(StreamError::other)?;
        might_fail_json()?; // via From<Box<dyn Error>>

        Ok(())
    }
}
