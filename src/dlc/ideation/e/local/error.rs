use std::{error::Error, fmt, rc::Rc};

#[derive(Debug, Clone)]
pub enum StreamError {
    Canceled,
    Aborted(Option<String>),
    Closing,
    Closed,
    Other(Rc<dyn Error>),
}

impl StreamError {
    /// Wrap any error type into `StreamError`
    pub fn other<E>(e: E) -> Self
    where
        E: Error + 'static,
    {
        StreamError::Other(Rc::new(e))
    }

    /// Wrap a boxed error
    pub fn other_boxed(e: Box<dyn Error>) -> Self {
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
        StreamError::Other(Rc::new(SimpleError(s.to_string())))
    }
}

impl From<String> for StreamError {
    fn from(s: String) -> Self {
        StreamError::from(s.as_str())
    }
}

impl From<std::io::Error> for StreamError {
    fn from(e: std::io::Error) -> Self {
        StreamError::Other(Rc::new(e))
    }
}

impl From<Box<dyn Error>> for StreamError {
    fn from(e: Box<dyn Error>) -> Self {
        StreamError::Other(e.into())
    }
}

#[macro_export]
macro_rules! impl_local_stream_error_from {
    ($($error_type:ty),* $(,)?) => {
        $(
            impl From<$error_type> for $crate::dlc::ideation::e::local::error::StreamError {
                fn from(e: $error_type) -> Self {
                    $crate::dlc::ideation::e::local::error::StreamError::Other(std::rc::Rc::new(e))
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

        impl_local_stream_error_from!(UserCustomError);

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
        fn might_fail_json() -> Result<(), Box<dyn Error>> {
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
        might_fail_json()?; // via From<Box<dyn Error>>

        Ok(())
    }
}
