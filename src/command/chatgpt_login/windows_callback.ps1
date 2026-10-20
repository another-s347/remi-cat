$ErrorActionPreference = 'Stop'
$listener = $null
try {
    Add-Type -AssemblyName System.Web
    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, __PORT__)
    $listener.Start()
    [Console]::Out.WriteLine((@{port=$listener.LocalEndpoint.Port} | ConvertTo-Json -Compress))
    [Console]::Out.Flush()
    # The pending OAuth state arrives through stdin, never the process command line.
    $config = [Console]::ReadLine() | ConvertFrom-Json
    if (!$config.state) { exit 1 }
    $deadline = [DateTime]::UtcNow.AddMinutes(30)
    while ([DateTime]::UtcNow -lt $deadline) {
        if (!$listener.Pending()) { Start-Sleep -Milliseconds 50; continue }
        $connection = $listener.AcceptTcpClient()
        $stream = $null
        try {
            $stream = $connection.GetStream()
            $stream.ReadTimeout = 3000
            $stream.WriteTimeout = 3000
            $request = [System.Text.StringBuilder]::new()
            while ($request.Length -lt 16384) {
                $byte = $stream.ReadByte()
                if ($byte -lt 0) { break }
                [void]$request.Append([char]$byte)
                if ($request.ToString().EndsWith("`r`n`r`n")) { break }
            }
            $first = ($request.ToString() -split "`r`n", 2)[0]
            $status = '400 Bad Request'
            $message = 'Invalid authorization callback. Return to the terminal and try again.'
            $target = $null
            $valid = $false
            if ($request.Length -lt 16384 -and $first -match '^GET (/auth/callback\?[^ ]+) HTTP/1\.[01]$') {
                $target = $Matches[1]
                $uri = [Uri]::new('http://127.0.0.1' + $target)
                $query = [System.Web.HttpUtility]::ParseQueryString($uri.Query)
                if ($query['state'] -ceq $config.state -and ($query['code'] -or $query['error'])) {
                    $valid = $true
                    $status = '200 OK'
                    $message = 'Authorization received. Return to remi-cat to see the result. You can close this tab.'
                }
            }
            $payload = [System.Text.Encoding]::UTF8.GetBytes($message)
            $headers = "HTTP/1.1 $status`r`nContent-Type: text/plain; charset=utf-8`r`nCache-Control: no-store`r`nReferrer-Policy: no-referrer`r`nContent-Length: $($payload.Length)`r`nConnection: close`r`n`r`n"
            $encoded = [System.Text.Encoding]::ASCII.GetBytes($headers)
            $stream.Write($encoded, 0, $encoded.Length)
            $stream.Write($payload, 0, $payload.Length)
            $stream.Flush()
            if ($valid) {
                [Console]::Out.WriteLine((@{callback=$target} | ConvertTo-Json -Compress))
                [Console]::Out.Flush()
                exit 0
            }
        } catch {
            # A malformed request or a browser disconnect must not consume the attempt.
        } finally {
            if ($stream) { $stream.Dispose() }
            $connection.Dispose()
        }
    }
} catch {
    # Do not print request targets or exception text containing OAuth data.
    [Console]::Out.WriteLine('{"error":"windows_callback_failed"}')
    [Console]::Out.Flush()
    exit 1
} finally {
    if ($listener) { $listener.Stop() }
}
