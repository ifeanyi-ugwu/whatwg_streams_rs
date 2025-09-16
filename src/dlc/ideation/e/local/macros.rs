/*#[macro_export]
macro_rules! localtest {
    ($($t:tt)*) => {
        #[tokio::test(flavor = "current_thread")]
        async fn $($t)* {
            let local = tokio::task::LocalSet::new();
            local.run_until(async { inner().await }).await;

            async fn inner() {
                $($t)*
            }
        }
    };
}*/

#[cfg(test)]
pub async fn local_test<F, Fut>(f: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let local = tokio::task::LocalSet::new();
    local.run_until(f()).await;
}
