using System.Diagnostics;
using System.IO.Compression;
using System.Runtime.InteropServices;

// Resolve the path where the native binary is cached.
var homeDir = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
var cacheDir = Path.Combine(homeDir, ".oly", "bin");
var binaryName = RuntimeInformation.IsOSPlatform(OSPlatform.Windows) ? "oly.exe" : "oly";
var binaryPath = Path.Combine(cacheDir, binaryName);

if (!File.Exists(binaryPath))
{
    await DownloadBinaryAsync(cacheDir, binaryName);
}

// Exec the native binary, passing all arguments through.
var psi = new ProcessStartInfo(binaryPath)
{
    UseShellExecute = false,
};
foreach (var arg in args)
    psi.ArgumentList.Add(arg);

var proc = Process.Start(psi)!;
await proc.WaitForExitAsync();
return proc.ExitCode;

// ---------------------------------------------------------------------------

static async Task DownloadBinaryAsync(string cacheDir, string binaryName)
{
    var version = typeof(Program).Assembly.GetName().Version?.ToString(3) ?? "0.1.0";
    var asset = GetAssetName();
    var url = $"https://github.com/slaveOftime/open-relay/releases/download/v{version}/{asset}";

    Console.Error.WriteLine($"oly: downloading native binary from {url}");
    Directory.CreateDirectory(cacheDir);

    var zipPath = Path.Combine(Path.GetTempPath(), asset);
    using (var http = new HttpClient())
    {
        http.DefaultRequestHeaders.Add("User-Agent", "oly-dotnet-tool");
        // Follow redirects (default behaviour of HttpClient).
        var bytes = await http.GetByteArrayAsync(url);
        await File.WriteAllBytesAsync(zipPath, bytes);
    }

    ZipFile.ExtractToDirectory(zipPath, cacheDir, overwriteFiles: true);
    File.Delete(zipPath);

    if (!RuntimeInformation.IsOSPlatform(OSPlatform.Windows))
    {
        // Mark executable on Unix.
        var binaryPath = Path.Combine(cacheDir, binaryName);
        var chmod = Process.Start(new ProcessStartInfo("chmod", $"+x \"{binaryPath}\"")
        {
            UseShellExecute = false,
        })!;
        await chmod.WaitForExitAsync();
    }

    Console.Error.WriteLine("oly: ready.");
}

static string GetAssetName()
{
    if (RuntimeInformation.IsOSPlatform(OSPlatform.Windows) &&
        RuntimeInformation.OSArchitecture == Architecture.X64)
        return "oly-windows-amd64.zip";

    if (RuntimeInformation.IsOSPlatform(OSPlatform.OSX) &&
        RuntimeInformation.OSArchitecture == Architecture.Arm64)
        return "oly-macos-arm64.zip";

    if (RuntimeInformation.IsOSPlatform(OSPlatform.Linux) &&
        RuntimeInformation.OSArchitecture == Architecture.X64)
        return "oly-linux-amd64.zip";

    throw new PlatformNotSupportedException(
        $"No pre-built binary for {RuntimeInformation.OSDescription} / " +
        $"{RuntimeInformation.OSArchitecture}. " +
        "Build from source: https://github.com/slaveOftime/open-relay");
}
