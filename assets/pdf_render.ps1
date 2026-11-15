param(
    [Parameter(Mandatory = $true)][string]$Pdf,
    [Parameter(Mandatory = $true)][string]$Out,
    [int]$Page = 1,
    [int]$Dpi = 150
)
# Rendert eine PDF-Seite mit dem in Windows 10/11 eingebauten PDF-Renderer
# (Windows.Data.Pdf) als PNG. Ausgabe: "<breite> <hoehe> <seitenzahl>" bei Erfolg.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Storage.StorageFile, Windows.Storage, ContentType = WindowsRuntime]
$null = [Windows.Data.Pdf.PdfDocument, Windows.Data.Pdf, ContentType = WindowsRuntime]
$null = [Windows.Storage.Streams.InMemoryRandomAccessStream, Windows.Storage.Streams, ContentType = WindowsRuntime]

$asTask = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
        $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and
        $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1' })[0]
function Await($op, [Type]$type) {
    $task = $asTask.MakeGenericMethod($type).Invoke($null, @($op))
    $task.Wait()
    return $task.Result
}
$asAction = ([System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
        $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and
        $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncAction' })[0]

$file = Await ([Windows.Storage.StorageFile]::GetFileFromPathAsync((Resolve-Path $Pdf).Path)) ([Windows.Storage.StorageFile])
$doc = Await ([Windows.Data.Pdf.PdfDocument]::LoadFromFileAsync($file)) ([Windows.Data.Pdf.PdfDocument])
$count = $doc.PageCount
if ($Page -lt 1 -or $Page -gt $count) { throw "Seite $Page gibt es nicht (1..$count)" }
$pg = $doc.GetPage([uint32]($Page - 1))
$size = $pg.Size                       # Geräteunabhängige Pixel = 1/96 Zoll
$scale = $Dpi / 96.0
$opts = New-Object Windows.Data.Pdf.PdfPageRenderOptions
$w = [uint32][Math]::Max(1, [Math]::Round($size.Width * $scale))
$h = [uint32][Math]::Max(1, [Math]::Round($size.Height * $scale))
$opts.DestinationWidth = $w
$opts.DestinationHeight = $h
$opts.BackgroundColor = [Windows.UI.Color]::FromArgb(255, 255, 255, 255)
$stream = New-Object Windows.Storage.Streams.InMemoryRandomAccessStream
$asAction.Invoke($null, @($pg.RenderToStreamAsync($stream, $opts))).Wait()
$stream.Seek(0)
$reader = [System.IO.WindowsRuntimeStreamExtensions]::AsStreamForRead($stream)
$fs = [System.IO.File]::Create($Out)
$reader.CopyTo($fs)
$fs.Close()
"$w $h $count"
