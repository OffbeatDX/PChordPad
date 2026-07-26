$ErrorActionPreference = "Stop"

$now = [DateTime]::UtcNow
$prefix = "$($now.Year).$($now.Month).$($now.Day)"
$pattern = "$prefix.*"
$escapedPrefix = [regex]::Escape($prefix)
$counter = 0

git tag --list $pattern | ForEach-Object {
    if ($_ -match "^$escapedPrefix\.(\d+)$") {
        $counter = [Math]::Max($counter, [int]$Matches[1])
    }
}

"$prefix.$($counter + 1)"
