param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$CommitTitle,

    [string]$CommitBody = '',
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$ExpectedRemote
)

$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($CommitTitle)) {
    throw 'A commit title is required.'
}
$blessValue = $env:FILMCRAFT_BLESS
if ($null -ne $blessValue) {
    throw 'FILMCRAFT_BLESS enables golden-reference writes; unset it before publishing so the quality gate compares the staged references.'
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

function Assert-RepositoryStateUnchanged([string]$Phase) {
    $currentBranch = & git -C $repo branch --show-current 2>$null
    $branchExit = $LASTEXITCODE
    $currentHead = & git -C $repo rev-parse HEAD 2>$null
    $headExit = $LASTEXITCODE
    $currentStagedTree = & git -C $repo write-tree 2>$null
    $treeExit = $LASTEXITCODE
    if ($branchExit -ne 0 -or $headExit -ne 0 -or $treeExit -ne 0 -or "$currentBranch".Trim() -cne $branch -or "$currentHead".Trim() -cne $headBeforeConfirmation -or "$currentStagedTree".Trim() -cne $stagedTreeBeforeConfirmation) {
        throw "The branch, HEAD, or staged changes changed $Phase; review and confirm again. Nothing was committed or pushed."
    }
    Assert-PushDestinationsUnchanged
    $unstagedChanges = @(& git -C $repo diff --name-only 2>$null)
    if ($LASTEXITCODE -ne 0) { throw "Could not inspect unstaged changes $Phase. Nothing was committed or pushed." }
    $untrackedChanges = @(& git -C $repo ls-files --others --exclude-standard 2>$null)
    if ($LASTEXITCODE -ne 0) { throw "Could not inspect untracked changes $Phase. Nothing was committed or pushed." }
    $workingTreeChanges = @($unstagedChanges + $untrackedChanges | Where-Object { -not [string]::IsNullOrWhiteSpace($_) } | Sort-Object -Unique)
    if ($workingTreeChanges.Count -gt 0) {
        throw "Unstaged or untracked changes exist $Phase. Nothing was committed or pushed:`n$($workingTreeChanges -join "`n")"
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
$headTreeBeforeConfirmation = & git -C $repo rev-parse "${headBeforeConfirmation}^{tree}" 2>$null
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
Write-Host 'The required FilmCraft quality gate runs in an isolated checkout of the staged tree before the commit and may take 20–60 minutes under load.'
Write-Host ''
$confirmation = Read-Host "Type YES to commit these files and push branch '$branch' to your GitHub fork"
if ($confirmation -cne 'YES') {
    Write-Host 'Cancelled. Nothing was committed or pushed.'
    exit 0
}

Assert-RepositoryStateUnchanged 'after confirmation'
$gateExitCode = 1
$gateCommit = & git -C $repo commit-tree $stagedTreeBeforeConfirmation -p $headBeforeConfirmation -m 'FilmCraft staged-tree quality gate'
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace("$gateCommit")) {
    throw 'Could not create the isolated staged-tree quality gate commit. Nothing was committed or pushed.'
}
$gateCommit = "$gateCommit".Trim()
$gateWorktree = Join-Path ([System.IO.Path]::GetTempPath()) ("filmcraft-quality-gate-" + [guid]::NewGuid().ToString('N'))
$gateWorktreeAdded = $false
$gateTrackedChanges = @()
$gateTrackedStatusExit = 0
try {
    & git -C $repo worktree add --detach $gateWorktree $gateCommit
    if ($LASTEXITCODE -ne 0) { throw 'Could not create the isolated staged-tree quality gate checkout. Nothing was committed or pushed.' }
    $gateWorktreeAdded = $true
    Push-Location -LiteralPath $gateWorktree
    try {
        & cargo --locked xtask ci
        $gateExitCode = $LASTEXITCODE
    }
    finally {
        Pop-Location
    }
}
finally {
    $gateWorktreeExists = Test-Path -LiteralPath $gateWorktree
    $registeredWorktrees = @(& git -C $repo worktree list --porcelain 2>$null)
    $gateWorktreeRegistered = $registeredWorktrees -contains "worktree $gateWorktree"
    if ($gateWorktreeAdded -or $gateWorktreeExists -or $gateWorktreeRegistered) {
        if ($gateWorktreeExists) {
            $gateTrackedChanges = @(& git -C $gateWorktree status --porcelain --untracked-files=no 2>$null)
            $gateTrackedStatusExit = $LASTEXITCODE
        }
        if ($gateTrackedStatusExit -ne 0) {
            throw 'Could not inspect tracked files in the isolated quality gate checkout. The checkout was kept for inspection; nothing was committed or pushed.'
        }
        if ($gateTrackedChanges.Count -gt 0) {
            throw "The quality gate changed tracked files in its isolated checkout. The checkout was kept for inspection; nothing was committed or pushed:`n$($gateTrackedChanges -join "`n")"
        }
        if ($gateExitCode -eq 0) {
            & git -C $repo worktree remove --force $gateWorktree
            if ($LASTEXITCODE -ne 0) { throw 'The isolated quality gate checkout could not be removed. Nothing was committed or pushed.' }
        }
    }
}
if ($gateTrackedStatusExit -ne 0) {
    throw 'Could not inspect tracked files in the isolated quality gate checkout. Nothing was committed or pushed.'
}
if ($gateTrackedChanges.Count -gt 0) {
    throw "The quality gate changed tracked files in its isolated checkout. Nothing was committed or pushed:`n$($gateTrackedChanges -join "`n")"
}
if ($gateExitCode -ne 0) {
    Write-Host "The failed quality-gate checkout and diagnostics were retained at: $gateWorktree"
    throw 'The FilmCraft quality gate failed. Nothing was committed or pushed.'
}
Assert-RepositoryStateUnchanged 'while the quality gate ran'
if ([string]::IsNullOrWhiteSpace($CommitBody)) {
    & git -C $repo commit -m $CommitTitle
} else {
    & git -C $repo commit -m $CommitTitle -m $CommitBody
}
if ($LASTEXITCODE -ne 0) { throw 'Commit failed; nothing was pushed.' }
$verifiedCommit = (& git -C $repo rev-parse HEAD).Trim()
$commitExit = $LASTEXITCODE
if ($commitExit -ne 0 -or [string]::IsNullOrWhiteSpace($verifiedCommit)) {
    throw 'Could not identify the committed revision. The commit is local; nothing was pushed.'
}
$committedTree = (& git -C $repo rev-parse "${verifiedCommit}^{tree}").Trim()
$treeExit = $LASTEXITCODE
$committedParent = (& git -C $repo rev-parse "${verifiedCommit}^").Trim()
$parentExit = $LASTEXITCODE
if ($treeExit -ne 0 -or $parentExit -ne 0 -or $committedTree -cne $stagedTreeBeforeConfirmation -or $committedParent -cne $headBeforeConfirmation) {
    throw 'A commit hook changed the committed tree or parent. The commit is local; nothing was pushed.'
}

Assert-PushDestinationsUnchanged
foreach ($pushUrl in $pushUrls) {
    & git -C $repo push --no-follow-tags --no-recurse-submodules $pushUrl "${verifiedCommit}:refs/heads/${branch}"
    if ($LASTEXITCODE -ne 0) { throw 'Commit succeeded, but pushing failed. The commit is still local or only partially published.' }
}

Write-Host ''
Write-Host "Published branch '$branch' to the configured FilmCraft fork."
