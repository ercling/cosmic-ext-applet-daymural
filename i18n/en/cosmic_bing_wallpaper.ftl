# Every user-visible string in the applet. Ids are referenced through the
# `fl!` macro, which fails the *compile* if an id is missing here — so this
# file is the authoritative inventory.

## Popup

# Opens Bing's own page about the displayed image in the browser.
about-this-image = About this image
# Heading fallback for images that carry no title yet (folder-scan rebuilds).
# English spells it as the product name (same casing as the desktop entry's
# `Name=`); translations may use a descriptive phrase instead.
bing-wallpaper = Bing Wallpaper

## Tooltips on the icon-only controls

tooltip-previous = Previous wallpaper
tooltip-next = Next wallpaper
tooltip-newest = Skip to newest
tooltip-refresh = Check for new images now
tooltip-open-image = Open image in viewer

## Settings rows

# Label of the "rotate through downloaded wallpapers" toggle.
shuffle = Shuffle
# Precedes the shuffle interval dropdown, reading "Every  1 hour".
shuffle-every = Every
# Precedes the retention dropdown, reading "Keep images  8 days".
keep-images = Keep images
# Label of the opt-in "derive the COSMIC accent colour from the current
# wallpaper" toggle; switching it off restores the previous accent.
match-accent-to-wallpaper = Match accent to wallpaper

## Shuffle interval dropdown

interval-30-minutes = 30 minutes
interval-1-hour = 1 hour
interval-6-hours = 6 hours
interval-daily = Daily

## Retention dropdown ("Forever" = never delete downloaded images)

retention-3-days = 3 days
retention-8-days = 8 days
retention-30-days = 30 days
retention-forever = Forever

## Status footer

status-checking = Checking for new images…
status-network-error = Bing unreachable — retrying in 1 h
status-disk-error = Disk error — retrying in 1 h
status-no-images = No images yet — fetching…
status-up-to-date = Up to date

# $time is a 24-hour clock time such as "09:12".
status-updated-today = Updated today at { $time }
status-updated-yesterday = Updated yesterday at { $time }
# $date is an English month abbreviation plus day, e.g. "Aug 5" — the date
# itself is not localized (that would pull in a full ICU date formatter);
# only this sentence frame is. Reorder the placeables as your language needs.
status-updated-on = Updated { $date } at { $time }
