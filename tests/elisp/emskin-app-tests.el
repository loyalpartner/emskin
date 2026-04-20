;;; emskin-app-tests.el --- Tests for emskin app lifecycle  -*- lexical-binding: t; -*-

(require 'ert)
(require 'emskin)

(ert-deftest emskin-on-window-destroyed-skips-nondeletable-main-window ()
  (let ((emskin--mirror-table (make-hash-table :test 'eql))
        (sent nil))
    (cl-letf (((symbol-function 'emskin--send)
               (lambda (msg) (setq sent msg))))
      (switch-to-buffer (get-buffer-create "*emskin-test*"))
      (display-buffer-in-side-window
       (get-buffer-create "*emskin-side*")
       '((side . right)))
      (with-current-buffer "*emskin-test*"
        (setq-local emskin--window-id 42))
      (should-not
       (condition-case err
           (progn
             (emskin--on-window-destroyed 42)
             nil)
         (error err)))
      (should-not (get-buffer "*emskin-test*"))
      (should (equal (alist-get 'type sent) "set_focus")))))

(ert-deftest emskin-open-native-app-queues-current-window ()
  (let ((emskin--pending-native-app-targets nil)
        started)
    (cl-letf (((symbol-function 'start-process)
               (lambda (&rest args)
                 (setq started args)
                 'fake-process)))
      (switch-to-buffer (get-buffer-create "*emskin-launch*"))
      (let ((win (selected-window)))
        (emskin-open-native-app "foot --app-id emskin-test")
        (should (equal emskin--pending-native-app-targets (list win)))
        (should (equal started
                       '("emskin-foot" nil "foot" "--app-id" "emskin-test")))))))

(ert-deftest emskin-on-window-created-reuses-queued-target-window ()
  (let ((emskin--pending-native-app-targets nil)
        reported)
    (cl-letf (((symbol-function 'emskin--report-geometry)
               (lambda (window-id window)
                 (setq reported (list window-id window)))))
      (switch-to-buffer (get-buffer-create "*emskin-origin*"))
      (let ((target (selected-window)))
        (setq emskin--pending-native-app-targets (list target))
        (emskin--on-window-created 7 "foot")
        (should (equal (buffer-name (window-buffer target))
                       "*emskin: foot*"))
        (should (equal (car reported) 7))
        (should (eq (cadr reported) target))
        (should-not emskin--pending-native-app-targets)))))

(provide 'emskin-app-tests)
;;; emskin-app-tests.el ends here
