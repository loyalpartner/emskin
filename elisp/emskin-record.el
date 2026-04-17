;;; emskin-record.el --- Screencast recording + screenshot commands  -*- lexical-binding: t; -*-

;; Two independent commands:
;;
;; - `emskin-toggle-record' — start/stop continuous video recording.  The
;;   compositor writes a fresh timestamped `.mp4' under
;;   `emskin-record-dir' every time the toggle flips on.  The macro-
;;   generated pattern (see `emskin-define-toggle' in `emskin-ipc.el') is
;;   the same as the other effect toggles, so `emskin-apply-config' /
;;   `emskin-connected-hook' both pick up the current state on reconnect.
;;
;; - `emskin-screenshot' — one-shot PNG capture, also under
;;   `emskin-record-dir'.  Unrelated to the video toggle's variable; you
;;   can take a screenshot while recording, and vice versa.

;;; Code:

(require 'emskin-ipc)

(defgroup emskin-record nil
  "emskin screencast and screenshot capture."
  :group 'emskin)

(defcustom emskin-record-dir "~/Videos/emskin/"
  "Directory where emskin recordings and screenshots are written."
  :type 'directory
  :group 'emskin-record)

(defcustom emskin-record-fps 30
  "Frame rate used when `emskin-toggle-record' starts a new video.
30 is a sensible default — 60 doubles the write bandwidth without a
proportional quality gain for UI content."
  :type 'integer
  :group 'emskin-record)

(defcustom emskin-screenshot-dir nil
  "Directory for `emskin-screenshot' files.  When nil, uses `emskin-record-dir'."
  :type '(choice (const :tag "Reuse record dir" nil) directory)
  :group 'emskin-record)

(defvar emskin-record)  ; declared in `emskin.el'

(defvar emskin--record-active-path nil
  "Path handed to the compositor on the most recent start.  `nil' when
the toggle is off.  Used only for the status message — the compositor
owns the real recording lifecycle.")

(defun emskin--record-ensure-dir (dir)
  (let ((d (expand-file-name dir)))
    (unless (file-directory-p d)
      (make-directory d t))
    d))

(defun emskin--record-video-path ()
  (expand-file-name
   (format-time-string "emskin-%Y%m%d-%H%M%S.mp4")
   (emskin--record-ensure-dir emskin-record-dir)))

(defun emskin--record-screenshot-path ()
  (expand-file-name
   (format-time-string "emskin-%Y%m%d-%H%M%S.png")
   (emskin--record-ensure-dir (or emskin-screenshot-dir emskin-record-dir))))

(defun emskin--record-sync ()
  "Push the current `emskin-record' state to the compositor.
Called by `emskin-toggle-record' and on every IPC (re-)connect via
`emskin-connected-hook'."
  (if emskin-record
      (let ((path (emskin--record-video-path)))
        (setq emskin--record-active-path path)
        (emskin--send `((type . "set_recording")
                        (enabled . t)
                        (path . ,path)
                        (fps . ,emskin-record-fps))))
    (setq emskin--record-active-path nil)
    (emskin--send '((type . "set_recording")
                    (enabled . :json-false)))))

(emskin-define-toggle record "recording")

;;;###autoload
(defun emskin-screenshot ()
  "Capture one PNG frame of the emskin output to `emskin-screenshot-dir'.
Independent of `emskin-toggle-record' — works while a video recording
is active and vice versa."
  (interactive)
  (let ((path (emskin--record-screenshot-path)))
    (emskin--send `((type . "take_screenshot") (path . ,path)))
    (message "emskin: screenshot → %s" path)))

(provide 'emskin-record)
;;; emskin-record.el ends here
