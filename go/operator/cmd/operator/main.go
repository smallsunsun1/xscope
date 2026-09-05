package main

import (
	"os"

	clientgoscheme "k8s.io/client-go/kubernetes/scheme"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/healthz"
	"sigs.k8s.io/controller-runtime/pkg/log/zap"
	metricsserver "sigs.k8s.io/controller-runtime/pkg/metrics/server"

	platformv1alpha1 "xscope.dev/xscope/api/v1alpha1"
	"xscope.dev/xscope/operator/internal/controller"
)

func main() {
	ctrl.SetLogger(zap.New(zap.UseDevMode(os.Getenv("XSCOPE_ENV") != "production")))
	scheme := clientgoscheme.Scheme
	if err := platformv1alpha1.AddToScheme(scheme); err != nil {
		panic(err)
	}

	manager, err := ctrl.NewManager(ctrl.GetConfigOrDie(), ctrl.Options{
		Scheme:                 scheme,
		Metrics:                metricsserver.Options{BindAddress: envOr("XSCOPE_OPERATOR_METRICS_ADDRESS", ":8083")},
		HealthProbeBindAddress: envOr("XSCOPE_OPERATOR_ADDRESS", ":8082"),
		LeaderElection:         true,
		LeaderElectionID:       "xscope-operator.platform.xscope.io",
	})
	if err != nil {
		panic(err)
	}
	if err := (&controller.ModelDeploymentReconciler{Client: manager.GetClient(), Scheme: manager.GetScheme()}).SetupWithManager(manager); err != nil {
		panic(err)
	}
	if err := manager.AddHealthzCheck("healthz", healthz.Ping); err != nil {
		panic(err)
	}
	if err := manager.AddReadyzCheck("readyz", healthz.Ping); err != nil {
		panic(err)
	}
	if err := manager.Start(ctrl.SetupSignalHandler()); err != nil {
		panic(err)
	}
}

func envOr(name, fallback string) string {
	if value := os.Getenv(name); value != "" {
		return value
	}
	return fallback
}
