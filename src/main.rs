use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Semaphore;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use anyhow::{Result, Context};
use clap::{Parser, CommandFactory};
use futures::TryStreamExt;
use std::sync::atomic::{AtomicBool, Ordering, AtomicU64};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinError;

// 添加全局中断标志
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

// 添加下载错误类型
#[derive(Debug)]
enum DownloadError {
    Interrupted,
    Timeout,
    NetworkError(String),
    IoError(String),
}

impl From<anyhow::Error> for DownloadError {
    fn from(err: anyhow::Error) -> Self {
        DownloadError::IoError(err.to_string())
    }
}

impl From<reqwest::Error> for DownloadError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            DownloadError::Timeout
        } else {
            DownloadError::NetworkError(err.to_string())
        }
    }
}

impl From<std::io::Error> for DownloadError {
    fn from(err: std::io::Error) -> Self {
        DownloadError::IoError(err.to_string())
    }
}

impl From<JoinError> for DownloadError {
    fn from(err: JoinError) -> Self {
        DownloadError::IoError(format!("任务执行错误: {}", err))
    }
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Interrupted => write!(f, "下载被用户中断"),
            Self::Timeout => write!(f, "下载超时"),
            Self::NetworkError(msg) => write!(f, "网络错误: {}", msg),
            Self::IoError(msg) => write!(f, "文件操作错误: {}", msg),
        }
    }
}

#[derive(Parser)]
#[command(
    name = env!("CARGO_PKG_NAME"),
    version = env!("CARGO_PKG_VERSION"),
    author = "云溪起源@唐溪",
    about = "多线程并发下载器",
    long_about = None,
    help_template = "{author} v{version} 制作的{about}\n\nUsage: {name} [OPTIONS] <URLS>...\n\nArguments:\n  <URLS>...  下载链接列表\n\nOptions:\n{options}",
)]
struct Cli {
    /// 下载链接列表
    #[arg(required_unless_present_any = ["version", "help"])]
    urls: Vec<String>,

    /// 下载线程数
    #[arg(short, long, default_value = "4", value_name = "THREADS")]
    threads: usize,

    /// 下载保存目录
    #[arg(short, long, default_value = ".", value_name = "DIR")]
    output_dir: PathBuf,

    /// 下载并发数
    #[arg(short = 'n', long = "num", default_value = "4", value_name = "NUM")]
    concurrent_downloads: usize,

    /// 安静模式
    #[arg(short, long)]
    quiet: bool,

    /// 打印版本信息
    #[arg(short = 'v', long = "version", help = "打印版本信息")]
    version: bool,

    /// 打印帮助菜单
    #[arg(short = 'h', long = "help", help = "打印帮助菜单")]
    help: bool,
}

struct Downloader {
    client: reqwest::Client,
    progress_bars: Arc<MultiProgress>,
    max_concurrent: Arc<Semaphore>,
    filename_counter: Arc<Mutex<HashMap<String, u32>>>,
    threads: usize,
}

impl Downloader {
    // 修改文件名生成逻辑
    fn generate_unique_filename(&self, original_name: &str) -> String {
        let mut counter = self.filename_counter.lock().unwrap();
        let count = counter.entry(original_name.to_string()).or_insert(0);
        *count += 1;
        
        if *count == 1 {
            original_name.to_string()
        } else {
            format!("{}_{}", count, original_name)
        }
    }

    // 添加一个从 URL 提取文件名的辅助函数
    fn extract_filename_from_url(url: &str) -> Option<String> {
        url.split('/')
            .last()
            .map(|s| s.to_string())
    }

    async fn download_chunk(
        client: reqwest::Client,
        url: String,
        start: u64,
        end: u64,
        progress: Arc<AtomicU64>,
        pb: ProgressBar,
    ) -> Result<Vec<u8>, DownloadError> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RANGE,
            format!("bytes={}-{}", start, end - 1).parse().unwrap(),
        );

        let response = client
            .get(&url)
            .headers(headers)
            .send()
            .await
            .map_err(|e| DownloadError::NetworkError(e.to_string()))?;

        if !response.status().is_success() {
            return Err(DownloadError::NetworkError(format!("HTTP状态码: {}", response.status())));
        }

        let mut chunk_data = Vec::with_capacity((end - start) as usize);
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.try_next().await? {
            if INTERRUPTED.load(Ordering::SeqCst) {
                return Err(DownloadError::Interrupted);
            }

            chunk_data.extend_from_slice(&chunk);
            let new_progress = progress.fetch_add(chunk.len() as u64, Ordering::SeqCst) + chunk.len() as u64;
            pb.set_position(new_progress);
        }

        Ok(chunk_data)
    }

    async fn download_file(&self, url: &str, output_dir: &PathBuf, thread_count: usize, quiet: bool) -> Result<(), DownloadError> {
        let response = self.client.get(url)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    DownloadError::Timeout
                } else {
                    DownloadError::NetworkError(e.to_string())
                }
            })?;
        
        let total_size = response
            .content_length()
            .context("无法获取文件大小")?;

        // 从 URL 中提取文件名
        let original_filename = Self::extract_filename_from_url(url)
            .ok_or_else(|| DownloadError::IoError("无法从 URL 提取文件名".to_string()))?;

        // 生成唯一的文件名
        let filename = self.generate_unique_filename(&original_filename);

        // 创建完整的输出路径
        let output_path = output_dir.join(&filename);

        let pb = self.progress_bars.add(ProgressBar::new(total_size));
        
        // 根据安静模式选择不同的进度条样式
        let style = if quiet {
            ProgressStyle::default_bar()
                .template("{msg}[{bar:40.cyan/blue}] {bytes}/{total_bytes} ({binary_bytes_per_sec} 剩余 {eta})")
                .unwrap()
                .progress_chars("=>-")
        } else {
            ProgressStyle::default_bar()
                .template("# [{elapsed_precise}] {msg}\n[{bar:40.cyan/blue}] {bytes}/{total_bytes} ({binary_bytes_per_sec}, 剩余 {eta})")
                .unwrap()
                .progress_chars("=>-")
        };

        pb.set_style(style);
        pb.enable_steady_tick(std::time::Duration::from_secs(1));
        pb.reset();

        // 在安静模式下只显示文件名
        if quiet {
            pb.set_message(format!("{} ", filename));
        } else {
            pb.set_message(format!("正在下载: {} (使用 {} 线程)", filename, thread_count));
        }

        // 检查是否支持断点续传
        let supports_range = response
            .headers()
            .get(reqwest::header::ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .map(|v| v == "bytes")
            .unwrap_or(false);

        if !supports_range {
            pb.println(format!("警告: {} 不支持断点续传，将使用单线程下载", filename));
        }

        if !supports_range || total_size < 1024 * 1024 { // 小于1MB的文件使用单线程下载
            // 使用现有的单线程下载逻辑
            let mut file = tokio::fs::File::create(&output_path)
                .await
                .context(format!("创建文件失败: {}", output_path.display()))?;

            let mut downloaded: u64 = 0;
            let mut stream = response.bytes_stream();
            let mut last_update = std::time::Instant::now();

            while let Ok(Some(chunk)) = stream.try_next().await {
                if INTERRUPTED.load(Ordering::SeqCst) {
                    pb.set_style(ProgressStyle::default_bar()
                        .template("# [{elapsed_precise}] {msg}\n[{bar:40.red/blue}] {bytes}/{total_bytes}")
                        .unwrap());
                    pb.finish_with_message(format!("❌ {} 下载被中断", filename));
                    let _ = tokio::fs::remove_file(&output_path).await;
                    return Err(DownloadError::Interrupted);
                }

                tokio::io::copy(&mut chunk.as_ref(), &mut file)
                    .await
                    .context("写入文件失败")?;
                
                downloaded += chunk.len() as u64;

                // 每秒更新一次进度
                let now = std::time::Instant::now();
                if now.duration_since(last_update).as_secs() >= 1 {
                    pb.set_position(downloaded);
                    last_update = now;
                }
            }

            // 确保最终进度被显示
            pb.set_position(downloaded);

            if INTERRUPTED.load(Ordering::SeqCst) {
                pb.set_style(ProgressStyle::default_bar()
                    .template("# [{elapsed_precise}] {msg}\n[{bar:40.red/blue}] {bytes}/{total_bytes}")
                    .unwrap());
                pb.finish_with_message(format!("❌ {} 下载被中断", filename));
                let _ = tokio::fs::remove_file(&output_path).await;
                return Err(DownloadError::Interrupted);
            }

            pb.finish_with_message(format!("✅ {} 下载完成", filename));
            Ok(())
        } else {
            // 修改分片大小计算逻辑
            let min_chunk_size = 1024 * 1024; // 最小分片大小为1MB
            let max_threads = if total_size < 10 * 1024 * 1024 {
                4 // 10MB以下最多4线程
            } else if total_size < 100 * 1024 * 1024 {
                8 // 100MB以下最多8线程
            } else if total_size < 1024 * 1024 * 1024 {
                16 // 1GB以下最多16线程
            } else {
                32 // 1GB以上最多32线程
            };

            // 如果用户指定了更大的线程数，使用用户指定的值
            let thread_count = if thread_count > max_threads {
                thread_count
            } else {
                max_threads
            };

            // 确保分片大小不小于最小值
            let chunk_size = std::cmp::max(
                (total_size + thread_count as u64 - 1) / thread_count as u64,
                min_chunk_size
            );

            // 直接创建分片下载任务
            let mut chunks = Vec::new();
            let mut start = 0;
            let progress = Arc::new(AtomicU64::new(0));

            // 创建分片下载任务
            while start < total_size {
                let end = std::cmp::min(start + chunk_size, total_size);
                let client = self.client.clone();
                let url = url.to_string();
                let progress = progress.clone();
                let pb = pb.clone();

                chunks.push(tokio::spawn(async move {
                    let mut retries = 3;
                    while retries > 0 {
                        match Self::download_chunk(client.clone(), url.clone(), start, end, progress.clone(), pb.clone()).await {
                            Ok(data) => return Ok((start, data)),
                            Err(e) => {
                                retries -= 1;
                                if retries == 0 {
                                    return Err(e);
                                }
                                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                            }
                        }
                    }
                    Err(DownloadError::NetworkError("下载失败".to_string()))
                }));

                start = end;
            }

            // 等待所有分片下载完成并按顺序写入文件
            let mut file = tokio::fs::File::create(&output_path).await?;
            let results = futures::future::join_all(chunks).await;
            
            // 检查是否被中断
            if INTERRUPTED.load(Ordering::SeqCst) {
                pb.finish_and_clear(); // 清除进度条
                return Err(DownloadError::Interrupted);
            }

            let mut chunks_data = Vec::new();
            for result in results {
                let (start, data) = result??; // 处理 JoinError 和 DownloadError
                chunks_data.push((start, data));
            }

            // 按照起始位置排序
            chunks_data.sort_by_key(|(start, _)| *start);

            // 写入文件
            for (_, data) in chunks_data {
                file.write_all(&data).await?;
            }

            file.flush().await?;
            pb.finish_with_message(format!("✅ {} 下载完成", filename));
            Ok(())
        }
    }

    async fn download_all(&self, urls: Vec<String>, output_dir: PathBuf, quiet: bool) -> Result<()> {
        if !quiet {
            println!("准备下载 {} 个文件", urls.len());
        }
        
        let mut tasks = Vec::new();
        let shared_progress = Arc::new(MultiProgress::new());
        let completed_files = Arc::new(Mutex::new(Vec::<PathBuf>::new()));

        // 智能计算线程数
        let calculate_thread_count = |file_size: u64, user_threads: usize| -> usize {
            // 基于文件大小的建议线程数
            let size_based_threads = match file_size {
                size if size < 1024 * 1024 => 1, // 1MB以下用1线程
                size if size < 10 * 1024 * 1024 => 4, // 10MB以下用4线程
                size if size < 100 * 1024 * 1024 => 8, // 100MB以下用8线程
                size if size < 1024 * 1024 * 1024 => 16, // 1GB以下用16线程
                _ => 32, // 1GB以上用32线程
            };

            // 如果用户指定了线程数，使用较大的值
            if user_threads > size_based_threads {
                user_threads
            } else {
                size_based_threads
            }
        };

        // 创建所有下载任务
        for url in urls {
            let client = self.client.clone();
            let output_dir = output_dir.clone();
            let filename_counter = self.filename_counter.clone();
            let progress_bars = shared_progress.clone();
            let user_threads = self.threads;
            let completed_files = completed_files.clone();
            let max_concurrent = self.max_concurrent.clone(); // 使用主 Downloader 的 Semaphore

            let task = tokio::spawn(async move {
                // 获取下载许可
                let _permit = max_concurrent.acquire().await.unwrap();

                // 先获取文件大小
                let size = match client.get(&url).send().await {
                    Ok(response) => response.content_length().unwrap_or(0),
                    Err(_) => 0,
                };

                let thread_count = calculate_thread_count(size, user_threads);

                let downloader = Downloader {
                    client,
                    progress_bars,
                    max_concurrent: Arc::new(Semaphore::new(thread_count)), // 这个是用于单个文件的多线程下载
                    filename_counter,
                    threads: user_threads,
                };

                let result = downloader.download_file(&url, &output_dir, thread_count, quiet).await;

                // 处理下载结果
                match &result {
                    Ok(_) => {
                        let mut completed = completed_files.lock().unwrap();
                        if let Some(name) = Self::extract_filename_from_url(&url) {
                            completed.push(output_dir.join(name));
                        }
                    }
                    Err(_) => {
                        if let Some(name) = Self::extract_filename_from_url(&url) {
                            let _ = tokio::fs::remove_file(output_dir.join(name)).await;
                        }
                    }
                }

                // 许可会在任务结束时自动释放
                (url, result)
            });

            tasks.push(task);
        }

        // 使用 tokio::select! 处理任务完成和中断信号
        let download_task = async {
            let results = futures::future::join_all(tasks).await;
            let mut errors = Vec::new();
            let mut has_error = false;
            let mut interrupted = false;

            for result in results {
                match result {
                    Ok((url, Err(e))) => {
                        has_error = true;
                        match e {
                            DownloadError::Interrupted => {
                                interrupted = true;
                                break;
                            }
                            DownloadError::Timeout => {
                                errors.push(format!("- {} 下载超时", url));
                            }
                            DownloadError::NetworkError(msg) => {
                                errors.push(format!("- {} {}", url, msg));
                            }
                            DownloadError::IoError(msg) => {
                                errors.push(format!("- {} {}", url, msg));
                            }
                        }
                    }
                    Err(e) => {
                        has_error = true;
                        errors.push(format!("- 任务异常: {}", e));
                    }
                    Ok((_, Ok(_))) => (),
                }
            }

            (interrupted, has_error, errors)
        };

        // 等待下载完成或中断信号
        tokio::select! {
            result = download_task => {
                let (interrupted, has_error, errors) = result;
                
                if interrupted {
                    eprintln!("\n下载被用户中断");
                    return Err(anyhow::anyhow!("下载被用户中断"));
                }

                if has_error {
                    eprintln!("\n下载任务失败，失败原因：");
                    for error in errors {
                        eprintln!("{}", error);
                    }
                    return Err(anyhow::anyhow!("下载失败"));
                }
            }
            _ = tokio::signal::ctrl_c() => {
                INTERRUPTED.store(true, Ordering::SeqCst);
                return Err(anyhow::anyhow!("下载被用户中断"));
            }
        }

        // 等待进度条更新完成
        shared_progress.clear()?;

        Ok(())
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 16)]
async fn main() -> Result<()> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(_) => {
            let mut cmd = Cli::command();
            cmd.print_help()?;
            println!();
            return Ok(());
        }
    };

    if cli.help {
        let mut cmd = Cli::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    }

    if cli.version {
        println!("pdm {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if !cli.output_dir.exists() {
        tokio::fs::create_dir_all(&cli.output_dir)
            .await
            .context("创建输出目录失败")?;
    }

    let downloader = Downloader {
        client: reqwest::Client::new(),
        progress_bars: Arc::new(MultiProgress::new()),
        max_concurrent: Arc::new(Semaphore::new(cli.concurrent_downloads)),
        filename_counter: Arc::new(Mutex::new(HashMap::new())),
        threads: cli.threads,
    };
    
    match downloader.download_all(cli.urls, cli.output_dir, cli.quiet).await {
        Ok(_) => {
            if !cli.quiet {
                println!("\n所有文件下载完成！");
            }
            Ok(())
        }
        Err(e) => {
            if INTERRUPTED.load(Ordering::SeqCst) {
                eprintln!("\n下载已中断");
            }
            Err(e)
        }
    }
} 