;;; emskin.el --- Emacs IPC client for the emskin Wayland compositor  -*- lexical-binding: t; -*-

(require 'cl-lib)

;; ---------------------------------------------------------------------------
;; Customization
;; ---------------------------------------------------------------------------

(defgroup emskin nil
  "Interface to the emskin nested Wayland compositor."
  :prefix "emskin-"
  :group 'applications)

(defcustom emskin-ipc-path nil
  "Explicit IPC socket path.  When nil, auto-discovered via parent PID."
  :type '(choice (const nil) string)
  :group 'emskin)

(defcustom emskin-measure nil
  "Non-nil to enable the measure overlay (Figma-style pixel inspector).
Shows a crosshair, coordinates label, and ruler strips on the
top/left edges of the output."
  :type 'boolean
  :group 'emskin
  :initialize #'custom-initialize-default
  :set (lambda (sym val)
         (set-default sym val)
         (when (bound-and-true-p emskin--process)
           (emskin--send `((type . "set_measure")
                               (enabled . ,(if val t :json-false)))))))

(defcustom emskin-skeleton nil
  "Non-nil to enable the skeleton overlay (frame layout inspector).
Draws wireframe rectangles around frame chrome, every window, its
header-line/mode-line, and the echo area with coordinates and sizes."
  :type 'boolean
  :group 'emskin
  :initialize #'custom-initialize-default
  :set (lambda (sym val)
         (set-default sym val)
         (when (bound-and-true-p emskin--process)
           (emskin--push-skeleton val))))

(defcustom emskin-demo-dir
  (expand-file-name
   "../demo"
   (file-name-directory
    (or load-file-name buffer-file-name
        "~/.emacs.d/site-lisp/emacs-application-framework/mvp/elisp/")))
  "Directory containing EAF demo/app Python scripts."
  :type 'directory
  :group 'emskin)

;; ---------------------------------------------------------------------------
;; Shared internal state (used across sub-modules)
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
(require 'emskin-skeleton)

;; ---------------------------------------------------------------------------
;; Public API: launch apps
;; ---------------------------------------------------------------------------

(defun emskin-toggle-measure ()
  "Toggle the measure overlay (crosshair + rulers)."
  (interactive)
  (customize-set-variable 'emskin-measure (not emskin-measure)))

(defvar emskin--cursor-trail nil)

(defun emskin-toggle-cursor-trail ()
  "Toggle the cursor trail effect."
  (interactive)
  (setq emskin--cursor-trail (not emskin--cursor-trail))
  (emskin--send `(("type" . "set_cursor_trail")
                  ("enabled" . ,emskin--cursor-trail)))
  (message "emskin: cursor trail %s" (if emskin--cursor-trail "ON" "OFF")))

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
