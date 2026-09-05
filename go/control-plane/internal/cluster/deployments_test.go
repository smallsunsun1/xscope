package cluster

import (
	"context"
	"testing"

	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"sigs.k8s.io/controller-runtime/pkg/client/fake"

	platformv1alpha1 "xscope.dev/xscope/api/v1alpha1"
)

func TestDeploymentLifecycle(t *testing.T) {
	scheme := runtime.NewScheme()
	if err := platformv1alpha1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	service := NewWithClient(fake.NewClientBuilder().WithScheme(scheme).Build())
	deployment := validDeployment()

	if err := service.Create(context.Background(), deployment); err != nil {
		t.Fatal(err)
	}
	items, err := service.List(context.Background(), "xscope-system")
	if err != nil {
		t.Fatal(err)
	}
	if len(items) != 1 || items[0].Name != "demo" {
		t.Fatalf("deployments = %#v", items)
	}
	scaled, err := service.Scale(context.Background(), "xscope-system", "demo", 3)
	if err != nil {
		t.Fatal(err)
	}
	if scaled.Spec.Replicas != 3 {
		t.Fatalf("replicas = %d, want 3", scaled.Spec.Replicas)
	}
	if err := service.Delete(context.Background(), "xscope-system", "demo"); err != nil {
		t.Fatal(err)
	}
}

func TestRejectsInvalidDeployment(t *testing.T) {
	scheme := runtime.NewScheme()
	if err := platformv1alpha1.AddToScheme(scheme); err != nil {
		t.Fatal(err)
	}
	service := NewWithClient(fake.NewClientBuilder().WithScheme(scheme).Build())
	deployment := validDeployment()
	deployment.Spec.Model.Checksum = "not-a-checksum"
	if err := service.Create(context.Background(), deployment); err == nil {
		t.Fatal("expected invalid checksum error")
	}
}

func validDeployment() *platformv1alpha1.ModelDeployment {
	return &platformv1alpha1.ModelDeployment{
		ObjectMeta: metav1.ObjectMeta{Name: "demo", Namespace: "xscope-system"},
		Spec: platformv1alpha1.ModelDeploymentSpec{
			Model: platformv1alpha1.ModelSpec{
				ID: "demo", Revision: "v1", URI: "s3://models/demo", Checksum: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
			},
			Runtime:  platformv1alpha1.RuntimeSpec{Image: "example/runtime:v1", Protocol: "openai", Port: 8000},
			Replicas: 1,
			Resources: corev1.ResourceRequirements{
				Requests: corev1.ResourceList{corev1.ResourceCPU: resource.MustParse("1")},
				Limits:   corev1.ResourceList{corev1.ResourceCPU: resource.MustParse("2")},
			},
		},
	}
}
