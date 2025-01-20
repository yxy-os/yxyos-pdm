# PDM - 多线程并发下载器

> Gitee: [https://gitee.com/yxyos/yxyos-pdm](https://gitee.com/yxyos/yxyos-pdm)<br>
> Github: [https://github.com/yxy-os/yxyos-pdm](https://github.com/yxy-os/yxyos-pdm)

由 @云溪起源@唐溪 开发的高性能多线程并发下载器。

## ✨ 特性

- 🚀 多线程分片下载：自动根据文件大小优化线程数
- 🔄 并发下载多个文件：支持同时下载多个文件
- 📥 断点续传：支持断点续传的文件自动使用多线程下载
- 📊 智能分片：根据文件大小自动调整分片大小和线程数
- 📈 进度显示：实时显示下载进度、速度和剩余时间
- 🛡️ 错误处理：支持网络错误重试和优雅中断

## 🚀 使用方法

```bash
pdm [选项] <URL>...
```

### 选项

- `-t, --threads <THREADS>`  下载线程数 [默认: 4]
- `-o, --output-dir <DIR>`   下载保存目录 [默认: .]
- `-n, --num <NUM>`          下载并发数 [默认: 4]
- `-q, --quiet`              安静模式
- `-v, --version`            打印版本信息
- `-h, --help`               打印帮助菜单