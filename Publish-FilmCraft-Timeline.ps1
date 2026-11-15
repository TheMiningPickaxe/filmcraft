param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$CommitTitle,

    [string]$CommitBody = '',
    [string]$ExpectedRemote = 'https://github.com/TheMiningPickaxe/filmcraft'
)

$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($CommitTitle)) {
    throw 'A commit title is required.'
}

function ConvertTo-CanonicalRepositoryUrl([string]$Url) {
    $value = $Url.Trim().TrimEnd('/')
    $value = $value -replace '^git@github\.com:', 'https://github.com/'
    $value = $value -replace '^ssh://git@github\.com/', 'https://github.com/'
    $value = $value -replace '\.git$', ''
    return $value.ToLowerInvariant()
}

$repo = & git -C $PSScriptRoot rev-parse --show-toplevel 2>$null
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace("$repo")) {
    throw 'The script must be inside a FilmCraft Git checkout.'
}
$repo = "$repo".Trim()

$pushUrls = @(& git -C $repo remote get-url --push --all origin)
if ($LASTEXITCODE -ne 0 -or $pushUrls.Count -eq 0) {
    throw 'Could not read the origin push destination.'
}
foreach ($url in $pushUrls) {
    if ((ConvertTo-CanonicalRepositoryUrl $url) -ne (ConvertTo-CanonicalRepositoryUrl $ExpectedRemote)) {
        throw 'The origin push destination is not the expected FilmCraft fork. Nothing was committed or pushed.'
    }
}

function Assert-PushDestinationsUnchanged {
    $currentUrls = @(& git -C $repo remote get-url --push --all origin)
    if ($LASTEXITCODE -ne 0 -or $currentUrls.Count -ne $pushUrls.Count) { throw 'Origin push destinations changed after confirmation.' }
    for ($i = 0; $i -lt $currentUrls.Count; $i++) {
        if ($currentUrls[$i] -cne $pushUrls[$i] -or (ConvertTo-CanonicalRepositoryUrl $currentUrls[$i]) -ne (ConvertTo-CanonicalRepositoryUrl $ExpectedRemote)) {
            throw 'Origin push destinations changed after confirmation.'
        }
    }
}

$branch = & git -C $repo branch --show-current 2>$null
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace("$branch")) {
    throw 'Could not determine the current branch.'
}
$branch = "$branch".Trim()
if ($branch -in @('main', 'master')) {
    throw "Expected a feature branch; current branch is '$branch'. Nothing was committed or pushed."
}

$headBeforeConfirmation = & git -C $repo rev-parse HEAD 2>$null
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace("$headBeforeConfirmation")) {
    throw 'Could not record the current commit before confirmation.'
}
$headBeforeConfirmation = "$headBeforeConfirmation".Trim()
$headTreeBeforeConfirmation = & git -C $repo rev-parse 'HEAD^{tree}' 2>$null
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace("$headTreeBeforeConfirmation")) {
    throw 'Could not record the current tree before confirmation.'
}
$headTreeBeforeConfirmation = "$headTreeBeforeConfirmation".Trim()
$stagedTreeBeforeConfirmation = & git -C $repo write-tree 2>$null
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace("$stagedTreeBeforeConfirmation")) {
    throw 'Could not record the staged tree before confirmation.'
}
$stagedTreeBeforeConfirmation = "$stagedTreeBeforeConfirmation".Trim()
$staged = @(& git -C $repo -c core.quotePath=false diff --name-only --no-renames $headTreeBeforeConfirmation $stagedTreeBeforeConfirmation)
if ($LASTEXITCODE -ne 0) { throw 'Could not inspect the recorded staged tree.' }
if ($staged.Count -eq 0) {
    throw 'There are no staged changes. Stage the Timeline files first; this script will not stage files automatically.'
}
$stagedPaths = @($staged | ForEach-Object { $_.Trim().Trim('"') })
$stagedInstructions = @($stagedPaths | Where-Object { $_ -match '(?i)(^|/)AGENTS\.md$' })
if ($stagedInstructions.Count -gt 0) {
    throw 'AGENTS.md is staged. Unstage it before publishing; this script will not commit it.'
}

Write-Host 'Staged files to be committed:'
$staged | ForEach-Object { Write-Host "  $_" }
Write-Host ''
Write-Host "Commit title: $CommitTitle"
if (-not [string]::IsNullOrWhiteSpace($CommitBody)) {
    Write-Host ''
    Write-Host $CommitBody
}
Write-Host ''
Write-Host 'The required FilmCraft quality gate runs before the commit and may take 20–60 minutes under load.'
Write-Host ''
$confirmation = Read-Host "Type YES to commit these files and push branch '$branch' to your GitHub fork"
if ($confirmation -cne 'YES') {
    Write-Host 'Cancelled. Nothing was committed or pushed.'
    exit 0
}

$currentBranch = & git -C $repo branch --show-current 2>$null
$branchExit = $LASTEXITCODE
$currentHead = & git -C $repo rev-parse HEAD 2>$null
$headExit = $LASTEXITCODE
$currentStagedTree = & git -C $repo write-tree 2>$null
$treeExit = $LASTEXITCODE
if ($branchExit -ne 0 -or $headExit -ne 0 -or $treeExit -ne 0 -or "$currentBranch".Trim() -cne $branch -or "$currentHead".Trim() -cne $headBeforeConfirmation -or "$currentStagedTree".Trim() -cne $stagedTreeBeforeConfirmation) {
    throw 'The branch, HEAD, or staged changes changed after confirmation. Nothing was committed or pushed; review and confirm again.'
}
Assert-PushDestinationsUnchanged
$runner = 'F:\Models\Runner\Runner.ps1'
if (-not (Test-Path -LiteralPath $runner -PathType Leaf)) {
    throw 'The FilmCraft quality gate runner is unavailable. Nothing was committed or pushed.'
}
& $runner 'cargo xtask ci' -W $repo
if ($LASTEXITCODE -ne 0) {
    throw 'The FilmCraft quality gate failed. Nothing was committed or pushed.'
}
$currentBranch = & git -C $repo branch --show-current 2>$null
$branchExit = $LASTEXITCODE
$currentHead = & git -C $repo rev-parse HEAD 2>$null
$headExit = $LASTEXITCODE
$currentStagedTree = & git -C $repo write-tree 2>$null
$treeExit = $LASTEXITCODE
if ($branchExit -ne 0 -or $headExit -ne 0 -or $treeExit -ne 0 -or "$currentBranch".Trim() -cne $branch -or "$currentHead".Trim() -cne $headBeforeConfirmation -or "$currentStagedTree".Trim() -cne $stagedTreeBeforeConfirmation) {
    throw 'The branch, HEAD, or staged changes changed while the quality gate ran. Nothing was committed or pushed; review and confirm again.'
}
Assert-PushDestinationsUnchanged
if ([string]::IsNullOrWhiteSpace($CommitBody)) {
    & git -C $repo commit -m $CommitTitle
} else {
    & git -C $repo commit -m $CommitTitle -m $CommitBody
}
if ($LASTEXITCODE -ne 0) { throw 'Commit failed; nothing was pushed.' }
$committedTree = (& git -C $repo rev-parse 'HEAD^{tree}').Trim()
$committedParent = (& git -C $repo rev-parse 'HEAD^').Trim()
if ($LASTEXITCODE -ne 0 -or $committedTree -cne $stagedTreeBeforeConfirmation -or $committedParent -cne $headBeforeConfirmation) {
    throw 'A commit hook changed the committed tree or parent. The commit is local; nothing was pushed.'
}
$verifiedCommit = (& git -C $repo rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($verifiedCommit)) {
    throw 'Could not verify the committed revision. The commit is local; nothing was pushed.'
}

Assert-PushDestinationsUnchanged
& git -C $repo push --no-follow-tags origin "${verifiedCommit}:refs/heads/${branch}"
if ($LASTEXITCODE -ne 0) { throw 'Commit succeeded, but pushing failed. The commit is still local.' }

Write-Host ''
Write-Host "Published branch '$branch' to the configured FilmCraft fork."
