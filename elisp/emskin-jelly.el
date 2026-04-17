;;; emskin-jelly.el --- Jelly text-cursor animation for emskin  -*- lexical-binding: t; -*-

;; Reports Emacs's text-cursor rectangle to the emskin compositor on every
;; `post-command-hook' tick.  The compositor renders a 200 ms jelly
;; animation from the previous rect to the new one.
;;
;; Algorithm ported from holo-layer's `holo-layer-get-cursor-info' — pure
;; elisp + IPC, no Wayland protocol dependency (pgtk does not broadcast
;; caret position via `text_input_v3' reliably).

;;; Code:

(require 'cl-lib)
(require 'emskin-app)  ; emskin--frame-header-offset

(declare-function emskin--send "emskin-ipc")

(defgroup emskin-jelly nil
  "Jelly text-cursor animation for emskin."
  :prefix "emskin-jelly-"
  :group 'emskin)

(defcustom emskin-jelly-cursor t
  "Non-nil to enable the jelly text-cursor animation."
  :type 'boolean
  :group 'emskin-jelly
  :initialize #'custom-initialize-default
  :set (lambda (sym val)
         (set-default sym val)
         (when (bound-and-true-p emskin--process)
           (emskin--send `((type . "set_jelly_cursor")
                           (enabled . ,(if val t :json-false)))))
         (if val
             (emskin-jelly--install-hooks)
           (emskin-jelly--remove-hooks))))

(defcustom emskin-jelly-fallback-color "#89dceb"
  "Fallback cursor color (hex) when no `cursor' face background is set.
Most themes set it; this rarely shows."
  :type 'string
  :group 'emskin-jelly)

(defvar emskin-jelly--last-info nil
  "Last sent `(WINDOW . \"x:y:w:h:color\")' pair.
The window component is a cheap tiebreaker so moving to a different
window at the same pixel coords still fires a new animation.")

;; ---------------------------------------------------------------------------
;; Cursor rect computation (holo-layer port)
;; ---------------------------------------------------------------------------

(defun emskin-jelly--window-pixel-edges (window)
  "Return (X Y) of WINDOW's top-left in Emacs surface coordinates.
Adds the GTK external menu/tool-bar offset so the result matches what
the compositor sees as the Emacs surface origin."
  (let ((edges (window-pixel-edges window)))
    (list (nth 0 edges)
          (+ (nth 1 edges) (emskin--frame-header-offset (window-frame window))))))

(defun emskin-jelly--cursor-rect ()
  "Return (X Y W H COLOR) of the current text cursor in surface pixels.
Returns nil when point is not visible in the selected window."
  (when-let* ((p (point))
              (window (selected-window))
              (vis (pos-visible-in-window-p p window t))
              (alloc (emskin-jelly--window-pixel-edges window)))
    (let* ((wx (nth 0 alloc))
           (wy (nth 1 alloc))
           (fringe-l (or (car (window-fringes window)) 0))
           (margin-l (or (car (window-margins window)) 0))
           (cw (frame-char-width))
           (cursor-w (if (eq cursor-type 'bar) 1 cw))
           (cursor-h (line-pixel-height))
           (x (+ (nth 0 vis) wx fringe-l (* margin-l cw)))
           (y (+ (nth 1 vis) wy))
           (color (or (face-background 'cursor nil t)
                      emskin-jelly-fallback-color)))
      (list x y cursor-w cursor-h color))))

;; ---------------------------------------------------------------------------
;; IPC + hook
;; ---------------------------------------------------------------------------

(defun emskin-jelly--send (info)
  "Send INFO (x y w h color) or nil for cancel."
  (let ((x (or (nth 0 info) 0))
        (y (or (nth 1 info) 0))
        (w (or (nth 2 info) 0))
        (h (or (nth 3 info) 0))
        (color (nth 4 info)))
    (emskin--send `((type . "set_cursor_rect")
                    (x . ,x) (y . ,y) (w . ,w) (h . ,h)
                    (color . ,(or color :null))))))

(defun emskin-jelly-monitor ()
  "Push the current cursor rect to the compositor if it moved."
  (when (and emskin-jelly-cursor
             (bound-and-true-p emskin--process))
    (let ((info (emskin-jelly--cursor-rect))
          (window (selected-window)))
      (cond
       ((null info)
        (when emskin-jelly--last-info
          (emskin-jelly--send nil)
          (setq emskin-jelly--last-info nil)))
       (t
        (let ((key (cons window
                         (format "%d:%d:%d:%d:%s"
                                 (nth 0 info) (nth 1 info)
                                 (nth 2 info) (nth 3 info) (nth 4 info)))))
          (unless (equal key emskin-jelly--last-info)
            (emskin-jelly--send info)
            (setq emskin-jelly--last-info key))))))))

(defun emskin-jelly--install-hooks ()
  (add-hook 'post-command-hook #'emskin-jelly-monitor))

(defun emskin-jelly--remove-hooks ()
  (remove-hook 'post-command-hook #'emskin-jelly-monitor)
  (setq emskin-jelly--last-info nil)
  (when (bound-and-true-p emskin--process)
    (emskin-jelly--send nil)))

(defun emskin-toggle-jelly-cursor ()
  "Toggle the jelly text-cursor animation."
  (interactive)
  (customize-set-variable 'emskin-jelly-cursor (not emskin-jelly-cursor))
  (message "emskin: jelly cursor %s" (if emskin-jelly-cursor "ON" "OFF")))

(when emskin-jelly-cursor
  (emskin-jelly--install-hooks))

(provide 'emskin-jelly)
;;; emskin-jelly.el ends here
