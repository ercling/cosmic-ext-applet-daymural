# Image rights and attribution

Daymural is an unofficial, independent project. It is not affiliated with,
authorized by, sponsored by, or endorsed by Microsoft. Native and Flatpak
packages contain no Bing homepage photographs; each installation fetches image
metadata and JPEGs directly from Bing.

Downloaded photographs remain the property of their respective rightsholders.
Use them only as personal, noncommercial wallpaper unless a rightsholder grants
broader permission. Do not redistribute a downloaded image merely because
Daymural made it available as wallpaper.

Microsoft states that most Bing daily images may be downloaded as wallpaper,
while licensing restrictions make some images unavailable. Its Services
Agreement limits Bing and MSN photos and other material to personal,
noncommercial use unless Microsoft, the relevant rightsholder, or applicable
copyright law permits more. See Microsoft's
[Bing homepage guidance](https://support.microsoft.com/en-us/bing/explore-the-homepage-1),
[Services Agreement](https://www.microsoft.com/en-us/servicesagreement#14f_BingandMSN),
and [copyright guidance](https://www.microsoft.com/en-us/legal/intellectualproperty/copyright/permissions).
Attribution identifies the rightsholder but does not grant additional rights.

## What Daymural records

For an image fetched with Bing metadata, Daymural saves the JPEG bytes exactly
as delivered. It stores the caption-derived title, credit, and information link
separately in `catalogue.json` under the applet's state directory. The popup
shows the credit and provides **About this image**.

The JPEG itself is not modified to contain that metadata. Copying the JPEG
alone therefore does not necessarily preserve the separate credit or link.

If the catalogue must be rebuilt from filenames, Daymural asks Bing for current
metadata again. It restores details for images that remain in Bing's
eight-image response window and are still marked downloadable. Older images
retain an honest filename-derived fallback when metadata is no longer
available.

## Download eligibility

Daymural honors Bing's per-image `wp` field and fails closed:

- an image is downloaded only when Bing reports `wp: true`;
- an image reported as `wp: false` is not downloaded;
- an absent `wp` field is not treated as permission to download; and
- an absent field alone never causes an existing image to be deleted.

If Bing explicitly marks a previously downloaded image ineligible, Daymural
removes its catalogue entry and JPEG. The image currently displayed is kept
until another wallpaper replaces it so the popup never shows credit for a
different image. On per-output wallpaper configurations, where Daymural cannot
reliably identify a single displayed image, it avoids eligibility-based
deletion entirely.

The metadata request uses Bing's supported eight-image window. Retention
settings determine which eligible images are downloaded. If the newest part of
the window contains no eligible image, Daymural can fetch the newest eligible
fallback in the remainder of that same window when it would also apply it. It
does not search older pages.

The `HPImageArchive` endpoint has no public third-party product license
identified by this project. Eligibility checks and attribution therefore do
not claim that every automated download or Store distribution has been
independently authorized. Publication remains subject to the applicable terms
and any permission required from Microsoft or the rightsholder.

Daymural's [GPL-3.0-only license](../LICENSE) covers its code and project-owned
assets, including its icon, metadata, translations, and project-owned
screenshot artwork. It does not cover photographs downloaded from Bing.
