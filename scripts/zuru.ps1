function Invoke-Zuru {
    [CmdletBinding()]
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$ZuruArgs)

    $cwdFile = [System.IO.Path]::GetTempFileName()
    try {
        $installed = Get-Command zuru -CommandType Application -ErrorAction SilentlyContinue
        $executable = if ($installed) {
            $installed.Source
        }
        else {
            Join-Path $PSScriptRoot '..\target\release\zuru.exe'
        }
        if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
            throw 'Zuru was not found in PATH and target\release\zuru.exe has not been built.'
        }
        & $executable @ZuruArgs --cwd-file $cwdFile
        $status = $LASTEXITCODE
        if (
            $status -eq 0 -and
            (Test-Path -LiteralPath $cwdFile) -and
            ((Get-Item -LiteralPath $cwdFile).Length -gt 0)
        ) {
            $destination = (Get-Content -LiteralPath $cwdFile -Raw).TrimEnd("`r", "`n")
            if ($destination -and (Test-Path -LiteralPath $destination -PathType Container)) {
                Set-Location -LiteralPath $destination
            }
        }
        return
    }
    finally {
        Remove-Item -LiteralPath $cwdFile -Force -ErrorAction SilentlyContinue
    }
}

Set-Alias -Name z -Value Invoke-Zuru
