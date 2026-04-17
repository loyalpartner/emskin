;;; emskin.el --- Emacs IPC client for the emskin Wayland compositor  -*- lexical-binding: t; -*-

;;; Code:

(require 'cl-lib)

;; ---------------------------------------------------------------------------
;; Connection settings
;; ---------------------------------------------------------------------------

(defvar emskin-ipc-path nil
  "Explicit IPC socket path.  When nil, auto-discovered via parent PID.")

(defvar emskin-demo-dir
  (expand-file-name
   "../demo"
   (file-name-directory
    (or load-file-name buffer-file-name
        "~/.emacs.d/site-lisp/emacs-application-framework/mvp/elisp/")))
  "Directory containing EAF demo/app Python scripts.")

;; ---------------------------------------------------------------------------
;; Effect toggles
;;
;; Each effect has its implementation in a sibling file.  Flip live with
;; the corresponding `emskin-toggle-*' command; set in your init file
;; (e.g. `~/.emacs.d/init.el') with `setq' and (optionally) re-apply
;; with `M-x emskin-apply-config' — or just wait for the next IPC
;; connect.
;; ---------------------------------------------------------------------------

(defvar emskin-measure nil
  "Non-nil to show the measure overlay — a Figma-style pixel inspector.
Crosshair at the mouse pointer, coordinates label next to it, and
ruler strips along the top/left edges.  Toggle with
`emskin-toggle-measure'.")

(defvar emskin-skeleton nil
  "Non-nil to show the skeleton overlay — a frame-layout debug inspector.
Wireframes around frame chrome, every window, header / mode-line,
echo-area, labels with kind + geometry; clicking a label flashes the
rect.  Toggle with `emskin-toggle-skeleton'.")

(defvar emskin-cursor-trail nil
  "Non-nil to show the cursor-trail effect — an elastic mouse-pointer trail.
Spring-damped circles follow the pointer and bounce back when it
stops.  Toggle with `emskin-toggle-cursor-trail'.")

(defvar emskin-jelly-cursor nil
  "Non-nil to show the jelly text-cursor animation.
On every command that moves point, a filled quadrilateral stretches
from the previous caret rect to the new one (200 ms), then collapses
into the new position.  Ported from holo-layer's `jelly' style.
Toggle with `emskin-toggle-jelly-cursor'.")

(defvar emskin-jelly-fallback-color "#89dceb"
  "Fallback jelly color when the `cursor' face has no background set.
Most themes set one, so this rarely shows.")

(defvar emskin-record nil
  "Non-nil while emskin is recording the composited output to a video file.
Toggle with `emskin-toggle-record'.  Each start writes a fresh
timestamped `.mp4' under `emskin-record-dir'.  `emskin-screenshot' is
a separate one-shot PNG capture that works independently.")

;; ---------------------------------------------------------------------------
;; Shared internal state
;; ---------------------------------------------------------------------------

(defvar emskin--process nil
  "The network process connected to emskin's IPC socket.")

(defvar emskin--read-buf ""
  "Accumulates raw bytes received from emskin.")

(defvar emskin--header-offset nil
  "Pixel height of external GTK bars (menu-bar + tool-bar).
Seeded once on the first compositor SurfaceSize event and kept
constant thereafter — it's a property of the Emacs GTK frame, not of
the compositor's surface, so re-measuring on every resize would race
with GTK and break app placement when a layer-shell bar appears or
disappears.")

(defvar-local emskin--window-id nil
  "emskin window_id for the embedded app in this buffer.")

(defvar-local emskin--visible nil
  "Whether this EAF buffer is currently displayed in an Emacs window.")

(defvar-local emskin--last-geometry nil
  "Last geometry sent for this buffer's EAF window, to skip no-op updates.")

(defvar emskin--mirror-table (make-hash-table :test 'eql)
  "Tracks source and mirror windows per embedded app.
Key: window-id.  Value: (SOURCE-WIN . ((VIEW-ID . EMACS-WIN) ...)).")

(defvar emskin--last-focused-wid 'unset
  "Last window-id sent via set_focus IPC.  Used as change-detection guard.")

(defvar emskin--next-view-id 0
  "Counter for generating unique mirror view IDs.")

;; --- Workspace tracking ---
(defvar emskin--frame-workspace-table (make-hash-table :test 'eq)
  "Maps Emacs frame objects to compositor workspace IDs.")

(defvar emskin--pending-frame-queue nil
  "Frames awaiting workspace_created IPC confirmation (FIFO order).")

(defvar emskin--active-workspace-id nil
  "Currently active workspace ID in the compositor.")

(defvar emskin--workspace-switch-suppressed nil
  "When non-nil, suppress workspace switch from after-focus-change.")

;; ---------------------------------------------------------------------------
;; Load sub-modules
;; ---------------------------------------------------------------------------

(require 'emskin-ipc)
(require 'emskin-app)
(require 'emskin-workspace)
(require 'emskin-measure)
(require 'emskin-skeleton)
(require 'emskin-cursor-trail)
(require 'emskin-jelly)
(require 'emskin-record)

;; ---------------------------------------------------------------------------
;; Config sync
;; ---------------------------------------------------------------------------

(defun emskin-apply-config ()
  "Re-push every registered effect's current value to the compositor.
Use after modifying variables with `setq'; toggle commands already
sync on every flip."
  (interactive)
  (unless emskin--process
    (user-error "emskin: not connected"))
  (run-hooks 'emskin-connected-hook)
  (message "emskin: config applied"))

;; ---------------------------------------------------------------------------
;; App launching
;; ---------------------------------------------------------------------------

(defun emskin-open-app (app-name)
  "Launch embedded application APP-NAME (Python script in `emskin-demo-dir')."
  (interactive "sApp name: ")
  (let ((script (expand-file-name (format "%s.py" app-name) emskin-demo-dir)))
    (unless (file-exists-p script)
      (error "EAF script not found: %s" script))
    (start-process (format "emskin-%s" app-name) nil "python3" script)
    (message "emskin: launched %s" app-name)))

(defun emskin-open-native-app (command)
  "Launch a native Wayland application inside emskin.
COMMAND is a shell command string, e.g. \"foot\" or \"firefox\"."
  (interactive "sCommand: ")
  (let ((args (split-string-and-unquote command)))
    (apply #'start-process
           (format "emskin-%s" (car args))
           nil args)
    (message "emskin: launched native app: %s" command)))

;; ---------------------------------------------------------------------------
;; Auto-connect when running inside emskin
;; ---------------------------------------------------------------------------

(defun emskin-maybe-auto-connect ()
  "Connect to emskin IPC if we appear to be running inside emskin.
Checks for the emskin-specific socket file derived from our parent PID."
  (let ((path (emskin--ipc-path)))
    (when (file-exists-p path)
      (run-with-timer 0.5 nil #'emskin-connect))))

(add-hook 'emacs-startup-hook #'emskin-maybe-auto-connect)

(provide 'emskin)
;;; emskin.el ends here
