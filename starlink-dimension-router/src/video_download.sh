delivery_request=__REQUEST__
delivery_url=__URL__
delivery_part=''
delivery_fail() {
    [ -z "$delivery_part" ] || rm -f -- "$delivery_part"
    printf 'SEEDANCE_DELIVERY_RECEIPT={"seedance_delivery":1,"request_id":"%s","status":"failed","error":"download_failed"}\n' "$delivery_request"
    exit 1
}
trap delivery_fail HUP INT TERM
delivery_directory=__DIRECTORY__
if [ -z "$delivery_directory" ] && command -v xdg-user-dir >/dev/null 2>&1; then delivery_directory=$(xdg-user-dir DOWNLOAD 2>/dev/null); fi
[ -n "$delivery_directory" ] || delivery_directory="$HOME/Downloads"
case "$delivery_directory" in /*) ;; *) delivery_fail ;; esac
mkdir -p -- "$delivery_directory" || delivery_fail
delivery_path="$delivery_directory/seedance-$delivery_request.mp4"
if [ -e "$delivery_path" ]; then
    delivery_path="$delivery_directory/seedance-$delivery_request-$(date +%s)-$$.mp4"
fi
delivery_part=$(mktemp "$delivery_directory/.seedance-$delivery_request.XXXXXX.part") || delivery_fail
# Do not follow redirects or echo the capability URL on error.
delivery_meta=$(curl --fail --silent --connect-timeout 15 --max-time 300 --max-filesize 4294967296 --proto '=http,https' --max-redirs 0 --output "$delivery_part" --write-out '%{http_code} %{content_type}' "$delivery_url" 2>/dev/null) || delivery_fail
case "$delivery_meta" in '200 video/mp4'*) ;; *) delivery_fail ;; esac
delivery_bytes=$(wc -c < "$delivery_part" | tr -d '[:space:]')
[ "$delivery_bytes" -ge 12 ] && [ "$delivery_bytes" -le 4294967296 ] || delivery_fail
delivery_header=$(od -An -tx1 -j4 -N4 "$delivery_part" | tr -d '[:space:]')
[ "$delivery_header" = '66747970' ] || delivery_fail
# Atomic no-replace publication, on the same filesystem as the temporary file.
ln -- "$delivery_part" "$delivery_path" || delivery_fail
rm -f -- "$delivery_part" || delivery_fail
delivery_part=''
delivery_escaped=$(printf '%s' "$delivery_path" | sed 's/\\/\\\\/g; s/"/\\"/g')
printf 'SEEDANCE_DELIVERY_RECEIPT={"seedance_delivery":1,"request_id":"%s","status":"saved","path":"%s","bytes":%s}\n' "$delivery_request" "$delivery_escaped" "$delivery_bytes"
