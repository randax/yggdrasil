#!/bin/sh

if [ "${LC_ALL:-}" = "C" ]; then
    printf '%s\n' 'fatal: unable to access repository: The requested URL returned error: 429' >&2
else
    printf '%s\n' 'schwerwiegend: Zugriff nicht möglich: Zu viele Anfragen' >&2
fi
exit 1
