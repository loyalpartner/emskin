;;; eaf-eafvil.el --- Emacs IPC client for the eafvil Wayland compositor  -*- lexical-binding: t; -*-

(require 'json)

;; ---------------------------------------------------------------------------
;; Customization
;; ---------------------------------------------------------------------------

(defgroup eaf-eafvil nil
  "Interface to the eafvil nested Wayland compositor."
  :prefix "eaf-eafvil-"
  :group 'applications)

(defcustom eaf-eafvil-ipc-path nil
  "Explicit IPC socket path.  When nil, auto-discovered via parent PID."
  :type '(choice (const nil) string)
  :group 'eaf-eafvil)

;; ---------------------------------------------------------------------------
;; Internal state
;; ---------------------------------------------------------------------------

(defvar eaf-eafvil--process nil
  "The network process connected to eafvil's IPC socket.")

(defvar eaf-eafvil--read-buf ""
  "Accumulates raw bytes received from eafvil.")

(defvar eaf-eafvil--header-offset nil
  "Pixel height of external GTK bars (menu-bar + tool-bar).
Computed once from compositor-reported surface height.")

(defvar-local eaf-eafvil--window-id nil
  "eafvil window_id for the EAF app embedded in this buffer.")

(defvar-local eaf-eafvil--visible nil
  "Whether this EAF buffer is currently displayed in an Emacs window.")

(defvar eaf-eafvil--displayed-table (make-hash-table :test 'eql)
  "Reusable hash-table for `eaf-eafvil--sync-all' to avoid per-call allocation.")

;; ---------------------------------------------------------------------------
;; Socket discovery
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--ipc-path ()
  "Return the IPC socket path, auto-discovering via parent PID when needed."
  (or eaf-eafvil-ipc-path
      (let* ((ppid (string-trim
                    (shell-command-to-string
                     (format "cat /proc/%d/status | awk '/^PPid:/{print $2}'"
                             (emacs-pid)))))
             (runtime-dir (or (getenv "XDG_RUNTIME_DIR") "/tmp")))
        (format "%s/eafvil-%s.ipc" runtime-dir ppid))))

;; ---------------------------------------------------------------------------
;; Codec: 4-byte u32 LE length prefix + JSON payload
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--encode-message (msg)
  "Encode MSG (alist/plist) as a framed JSON message (unibyte string)."
  (let* ((json (encode-coding-string (json-encode msg) 'utf-8 t))
         (len (length json))
         (prefix (unibyte-string
                  (logand len #xff)
                  (logand (ash len -8) #xff)
                  (logand (ash len -16) #xff)
                  (logand (ash len -24) #xff))))
    (concat prefix json)))

(defun eaf-eafvil--decode-next ()
  "Extract one complete message from `eaf-eafvil--read-buf'.
Returns parsed JSON (hash-table) or nil if more data is needed.
Coerces buffer to unibyte so aref always yields raw byte values 0-255."
  (when (>= (length eaf-eafvil--read-buf) 4)
    (let* ((b0 (aref eaf-eafvil--read-buf 0))
           (b1 (aref eaf-eafvil--read-buf 1))
           (b2 (aref eaf-eafvil--read-buf 2))
           (b3 (aref eaf-eafvil--read-buf 3))
           (len (+ b0 (ash b1 8) (ash b2 16) (ash b3 24))))
      (when (>= (length eaf-eafvil--read-buf) (+ 4 len))
        (let* ((payload (decode-coding-string
                         (substring eaf-eafvil--read-buf 4 (+ 4 len)) 'utf-8))
               (obj (json-parse-string payload)))
          (setq eaf-eafvil--read-buf
                (substring eaf-eafvil--read-buf (+ 4 len)))
          obj)))))

;; ---------------------------------------------------------------------------
;; Process filter (calloop equivalent on the Emacs side)
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--filter (proc data)
  "Accumulate DATA from PROC and dispatch complete messages."
  (ignore proc)
  (setq eaf-eafvil--read-buf
        (concat eaf-eafvil--read-buf (string-as-unibyte data)))
  (let (msg)
    (while (setq msg (eaf-eafvil--decode-next))
      (eaf-eafvil--dispatch msg))))

(defun eaf-eafvil--sentinel (proc event)
  "Handle IPC connection state changes."
  (when (string-match-p "\\(closed\\|failed\\|broken\\|finished\\)" event)
    (message "eafvil: IPC connection %s" (string-trim event))
    (setq eaf-eafvil--process nil)))

;; ---------------------------------------------------------------------------
;; Message dispatch
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--dispatch (msg)
  "Dispatch a parsed MSG hash-table from eafvil."
  (let ((type (gethash "type" msg "")))
    (cond
     ((string= type "connected")
      (message "eafvil: connected (version %s)" (gethash "version" msg "?")))
     ((string= type "error")
      (message "eafvil error: %s" (gethash "msg" msg "")))
     ((string= type "window_created")
      (eaf-eafvil--on-window-created (gethash "window_id" msg)
                                  (gethash "title" msg "")))
     ((string= type "window_destroyed")
      (eaf-eafvil--on-window-destroyed (gethash "window_id" msg)))
     ((string= type "title_changed")
      (eaf-eafvil--on-title-changed (gethash "window_id" msg)
                                 (gethash "title" msg "")))
     ((string= type "surface_size")
      (let* ((h (gethash "height" msg))
             (offset (max 0 (- h (frame-pixel-height)))))
        (setq eaf-eafvil--header-offset offset)
        (message "eafvil: surface=%sx%s bars=%dpx"
                 (gethash "width" msg) h offset)
        ;; Re-sync all EAF windows now that we have the correct offset.
        (dolist (frame (frame-list))
          (eaf-eafvil--sync-all frame))))
     (t
      (message "eafvil: unknown message type %s" type)))))

(defun eaf-eafvil--on-window-created (window-id title)
  "Create/display a buffer for the new EAF app and send initial geometry."
  (let* ((buf-name (format "*eaf: %s*" (if (string-empty-p title) "app" title)))
         (buf (get-buffer-create buf-name)))
    (with-current-buffer buf
      (setq-local eaf-eafvil--window-id window-id)
      (setq-local mode-name "EAF")
      (setq-local buffer-read-only t)
      (add-hook 'kill-buffer-hook #'eaf-eafvil--kill-buffer-hook nil t))
    (switch-to-buffer buf)
    (when-let ((win (get-buffer-window buf t)))
      (eaf-eafvil--report-geometry window-id win))
    (message "eafvil: EAF app ready (id=%s)" window-id)))

(defun eaf-eafvil--find-buffer (window-id)
  "Return the buffer whose `eaf-eafvil--window-id' equals WINDOW-ID, or nil."
  (seq-find (lambda (buf)
              (equal (buffer-local-value 'eaf-eafvil--window-id buf) window-id))
            (buffer-list)))

(defun eaf-eafvil--on-window-destroyed (window-id)
  "Kill the EAF buffer associated with WINDOW-ID."
  (when-let ((buf (eaf-eafvil--find-buffer window-id)))
    ;; Clear window-id first to prevent kill-buffer-hook from sending
    ;; a redundant "close" message back to the compositor.
    (with-current-buffer buf
      (setq-local eaf-eafvil--window-id nil))
    (kill-buffer buf)
    (message "eafvil: window %s destroyed" window-id)))

(defun eaf-eafvil--on-title-changed (window-id title)
  "Rename the EAF buffer when the app title changes."
  (when-let ((buf (eaf-eafvil--find-buffer window-id)))
    (with-current-buffer buf
      (rename-buffer (format "*eaf: %s*" title) t))))

;; ---------------------------------------------------------------------------
;; Lifecycle: kill-buffer → close
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--kill-buffer-hook ()
  "Notify eafvil to close the app when its Emacs buffer is killed."
  (when eaf-eafvil--window-id
    (eaf-eafvil--send `((type . "close")
                        (window_id . ,eaf-eafvil--window-id)))))

;; ---------------------------------------------------------------------------
;; Public API
;; ---------------------------------------------------------------------------

(defun eaf-eafvil-connect ()
  "Connect to the eafvil IPC socket (auto-discovers path)."
  (interactive)
  (when eaf-eafvil--process
    (delete-process eaf-eafvil--process)
    (setq eaf-eafvil--process nil))
  (setq eaf-eafvil--read-buf "")
  (let ((path (eaf-eafvil--ipc-path)))
    (condition-case err
        (progn
          (setq eaf-eafvil--process
                (make-network-process
                 :name "eaf-eafvil-ipc"
                 :family 'local
                 :service path
                 :coding 'binary
                 :filter #'eaf-eafvil--filter
                 :sentinel #'eaf-eafvil--sentinel
                 :nowait nil))
          (message "eafvil: connecting to %s" path))
      (error
       (message "eafvil: failed to connect to %s: %s" path err)))))

(defun eaf-eafvil--send (msg)
  "Send MSG (alist) to eafvil over IPC."
  (when eaf-eafvil--process
    (process-send-string eaf-eafvil--process (eaf-eafvil--encode-message msg))))

;; ---------------------------------------------------------------------------
;; Geometry reporting
;; ---------------------------------------------------------------------------

(defun eaf-eafvil--frame-header-offset (&optional _frame)
  "Pixel height of external GTK bars (menu-bar + tool-bar).
Computed once when the compositor reports the surface size."
  (or eaf-eafvil--header-offset 0))

(defun eaf-eafvil--window-geometry (window)
  "Return (x y w h) in pixels for Emacs WINDOW.
Coordinates are relative to the top-left of the Wayland surface.
Covers the full window width (including fringes) but excludes the mode-line."
  (let* ((edges (window-pixel-edges window))
         (body-edges (window-body-pixel-edges window))
         (x (nth 0 edges))
         (raw-y (nth 1 edges))
         (y (+ raw-y (eaf-eafvil--frame-header-offset (window-frame window))))
         (w (- (nth 2 edges) x))
         ;; body-bottom = top of mode-line; stop there so mode-line stays visible.
         (h (- (nth 3 body-edges) raw-y)))
    (list x y w h)))

(defun eaf-eafvil-debug-geometry ()
  "Print geometry debug info to *Messages*."
  (interactive)
  (let* ((frame (selected-frame))
         (geom (frame-geometry frame))
         (win (selected-window))
         (root-edges (window-pixel-edges (frame-root-window frame)))
         (mb-h (or (cdr (alist-get 'menu-bar-size geom)) 0))
         (tb-h (or (cdr (alist-get 'tool-bar-size geom)) 0))
         (mb-ext (alist-get 'menu-bar-external geom))
         (tb-ext (alist-get 'tool-bar-external geom))
         (outer-h (cdr (alist-get 'outer-size geom)))
         (pixel-h (frame-pixel-height frame))
         (inner-h (frame-inner-height frame))
         (mb-lines (frame-parameter frame 'menu-bar-lines))
         (offset (eaf-eafvil--frame-header-offset frame))
         (final (eaf-eafvil--window-geometry win)))
    (message (concat "eafvil-debug: "
                     "mb: h=%d ext=%s lines=%s | "
                     "tb: h=%d ext=%s | "
                     "outer-h=%s pixel-h=%d inner-h=%d | "
                     "root-edges: %s | "
                     "offset: %d | final: %s")
             mb-h mb-ext mb-lines
             tb-h tb-ext
             outer-h pixel-h inner-h
             root-edges offset final)))

(defvar-local eaf-eafvil--last-geometry nil
  "Last geometry sent for this buffer's EAF window, to skip no-op updates.")

(defun eaf-eafvil--report-geometry (window-id window)
  "Send set_geometry for WINDOW-ID, only when geometry actually changed."
  (let ((geo (eaf-eafvil--window-geometry window)))
    (unless (equal geo (buffer-local-value 'eaf-eafvil--last-geometry
                                           (window-buffer window)))
      (with-current-buffer (window-buffer window)
        (setq-local eaf-eafvil--last-geometry geo))
      (eaf-eafvil--send `((type . "set_geometry")
                      (window_id . ,window-id)
                      (x . ,(nth 0 geo))
                      (y . ,(nth 1 geo))
                      (w . ,(nth 2 geo))
                      (h . ,(nth 3 geo)))))))

(defun eaf-eafvil--sync-all (_frame)
  "Sync visibility and geometry for all EAF buffers across all frames."
  (let ((displayed eaf-eafvil--displayed-table))
    (clrhash displayed)
    ;; Pass 1: collect currently displayed EAF window-ids.
    (dolist (fr (frame-list))
      (dolist (win (window-list fr 'no-minibuf))
        (when-let ((wid (buffer-local-value 'eaf-eafvil--window-id
                                            (window-buffer win))))
          (unless (gethash wid displayed)
            (puthash wid win displayed)))))
    ;; Pass 2: update visibility and geometry for every EAF buffer.
    (dolist (buf (buffer-list))
      (when-let ((wid (buffer-local-value 'eaf-eafvil--window-id buf)))
        (let* ((win (gethash wid displayed))
               (now-visible (and win t))
               (was-visible (buffer-local-value 'eaf-eafvil--visible buf)))
          ;; Send set_visibility only when state changed.
          (unless (eq now-visible was-visible)
            (with-current-buffer buf
              (setq-local eaf-eafvil--visible now-visible))
            (eaf-eafvil--send `((type . "set_visibility")
                                (window_id . ,wid)
                                (visible . ,(if now-visible t :false)))))
          ;; Sync geometry for visible windows.
          (when win
            (eaf-eafvil--report-geometry wid win)))))))

(add-hook 'window-size-change-functions #'eaf-eafvil--sync-all)
(add-hook 'window-buffer-change-functions #'eaf-eafvil--sync-all)

;; ---------------------------------------------------------------------------
;; Launch an EAF application
;; ---------------------------------------------------------------------------

(defcustom eaf-eafvil-demo-dir
  (expand-file-name
   "../demo"
   (file-name-directory
    (or load-file-name buffer-file-name
        "~/.emacs.d/site-lisp/emacs-application-framework/mvp/elisp/")))
  "Directory containing EAF demo/app Python scripts."
  :type 'directory
  :group 'eaf-eafvil)

(defun eaf-open-app (app-name)
  "Launch EAF application APP-NAME (Python script in `eaf-eafvil-demo-dir')."
  (interactive "sApp name: ")
  (let* ((script (expand-file-name (format "%s.py" app-name) eaf-eafvil-demo-dir))
         (process-environment
          (cons (format "WAYLAND_DISPLAY=%s" (or (getenv "WAYLAND_DISPLAY") ""))
                process-environment)))
    (unless (file-exists-p script)
      (error "EAF script not found: %s" script))
    (start-process (format "eaf-%s" app-name) nil "python3" script)
    (message "eafvil: launched %s" app-name)))

;; ---------------------------------------------------------------------------
;; Auto-connect when running inside eafvil
;; ---------------------------------------------------------------------------

(defun eaf-eafvil-maybe-auto-connect ()
  "Connect to eafvil IPC if we appear to be running inside eafvil.
Checks for the eaf-eafvil-specific socket file derived from our parent PID."
  (when (featurep 'pgtk)
    (let ((path (eaf-eafvil--ipc-path)))
      (when (file-exists-p path)
        (run-with-timer 0.5 nil #'eaf-eafvil-connect)))))

;; Hook into Emacs startup.
(add-hook 'emacs-startup-hook #'eaf-eafvil-maybe-auto-connect)

(provide 'eaf-eafvil)
;;; eaf-eafvil.el ends here
