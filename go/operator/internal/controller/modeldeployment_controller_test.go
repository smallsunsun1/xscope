package controller

import (
	"context"
	"testing"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/types"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"

	platformv1alpha1 "xscope.dev/xscope/api/v1alpha1"
)

func TestReconcileCreatesDeploymentAndService(t *testing.T) {
	scheme := runtime.NewScheme()
	if err := appsv1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	if err := corev1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	if err := platformv1alpha1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	model := &platformv1alpha1.ModelDeployment{
		ObjectMeta: metav1.ObjectMeta{Name: "demo", Namespace: "default"},
		Spec: platformv1alpha1.ModelDeploymentSpec{
			Model:    platformv1alpha1.ModelSpec{ID: "demo", Revision: "v1", URI: "s3://models/demo", Checksum: "sha256:abc"},
			Runtime:  platformv1alpha1.RuntimeSpec{Image: "runtime:test", Port: 8000},
			Replicas: 2,
		},
	}
	kubeClient := fake.NewClientBuilder().WithScheme(scheme).WithStatusSubresource(model).WithObjects(model).Build()
	reconciler := &ModelDeploymentReconciler{Client: kubeClient, Scheme: scheme}
	if _, err := reconciler.Reconcile(context.Background(), ctrl.Request{NamespacedName: types.NamespacedName{Name: "demo", Namespace: "default"}}); err != nil {
		t.Fatal(err)
	}
	var deployment appsv1.Deployment
	if err := kubeClient.Get(context.Background(), types.NamespacedName{Name: "demo", Namespace: "default"}, &deployment); err != nil {
		t.Fatal(err)
	}
	if deployment.Spec.Replicas == nil || *deployment.Spec.Replicas != 2 {
		t.Fatalf("replicas = %v", deployment.Spec.Replicas)
	}
	var service corev1.Service
	if err := kubeClient.Get(context.Background(), types.NamespacedName{Name: "demo", Namespace: "default"}, &service); err != nil {
		t.Fatal(err)
	}
	if service.Spec.Ports[0].Port != 8000 {
		t.Fatalf("port = %d", service.Spec.Ports[0].Port)
	}
}
